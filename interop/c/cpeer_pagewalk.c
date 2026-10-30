/* cpeer_pagewalk.c - check-freelist: audit the page usage of a snapshot.
 *
 * Every page 0..last_pg of a committed snapshot is used exactly once (see
 * the comment above MDB_page in mdb.c, and mdb_audit()):
 *   - pages 0 and 1 are the meta pages;
 *   - a page of a B-tree: branch, leaf or overflow page of the free DB, the
 *     main DB, a named DB, or a DUPSORT sub-DB;
 *   - or it is listed in a free DB record (txnid -> [count, pgno...]).
 * Anything else is a leak (page used by nothing) or a double use, and a C
 * writer meeting such a file would sooner or later corrupt data.
 *
 * The walk reads the data file with pread() and decodes the on-disk format
 * at the offsets documented in rust/DESIGN.md (LMDB 0.9) and
 * rust/DUAL_FORMAT.md (LMDB 1.0), independently of LMDB's own code, so it
 * doubles as a check of that documentation. The snapshot is pinned with an
 * LMDB read transaction, which keeps writers from reusing its pages while
 * we read them. It also checks every B-tree descriptor (MDB_db) against
 * what the walk found: depth, page counts, entries.
 *
 * The format is the one of the library this file is compiled against
 * (cpeer: LMDB 0.9, cpeer10: LMDB 1.0), checked at run time in the meta
 * pages. cpeer_capi (built with LMDB_RUST, on rlmdb, which opens both)
 * takes it from meta page 0 instead. A 1.0 snapshot is also checked for what only 1.0 records: every
 * page reachable from it carries the txnid of the transaction that wrote
 * it (mp_txnid in 1..snapshot; not meta pages, which carry 0, nor
 * sub-pages, which the compacting copy leaves as they were while it
 * stamps every page 1), and the MDB_ovpage of an F_BIGDATA node agrees
 * with its overflow run (op_pages == mp_pages, op_txnid in 1..mp_txnid:
 * an in-place overwrite restamps the page but not the reference).
 */
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "cpeer.h"

/* ---- On-disk format (host byte order; see DESIGN.md) -------------------- */

/* What differs between the two data-file formats (rust/DUAL_FORMAT.md §1).
 * The node header, MDB_db and MDB_meta are the same in both; so are the
 * last 8 bytes of the page header (mp_pad, mp_flags, mp_lower/mp_upper or
 * mp_pages), which 1.0 moves up by the 8 bytes of mp_txnid. */
struct layout {
	const char	*name;		/* the release that writes it */
	uint32_t	version;	/* mm_version: MDB_DATA_VERSION */
	unsigned int	hdr;		/* PAGEHDRSZ; the MDB_meta of pages 0 and 1 follows it */
	unsigned int	base;		/* PAGEBASE: what mp_lower, mp_upper, mp_ptrs[] count from */
	unsigned int	has_txnid;	/* mp_txnid (u64) at offset 8 */
	unsigned int	even_keys;	/* NODEDATA pads the key to an even length */
	unsigned int	ovpage;		/* F_BIGDATA node data: pgno_t, or MDB_ovpage */
	uint16_t	subp_dirty;	/* P_DIRTY, which 0.9 leaves in sub-page flags */
};

static const struct layout layouts[2] = {
	/* LMDB 0.9, MDB_DATA_VERSION 1 */
	{ "LMDB 0.9", 1, 16, 0, 0, 0, 8, 0x10 },
	/* LMDB 1.0, MDB_DATA_VERSION 3: mp_txnid after mp_pgno, offsets in the
	 * page relative to the end of the header (ITS#7713), MDB_ovpage
	 * {pgno, txnid, pages}, no P_DIRTY */
	{ "LMDB 1.0", 3, 24, 24, 1, 1, 24, 0 },
};

/* The format of the lmdb.h this file is compiled against; on rlmdb
 * (LMDB_RUST) the one meta page 0 is in (choose_format()). */
#if MDB_VERSION_MAJOR == 0
#define FORMAT		0
#elif MDB_VERSION_MAJOR == 1
#define FORMAT		1
#else
#error "cpeer_pagewalk.c decodes the data files of LMDB 0.9 and 1.0 only"
#endif
static const struct layout *fmt = &layouts[FORMAT];
static const struct layout *other_fmt = &layouts[1 - FORMAT];

#define MAX_PAGEHDRSZ	24
#define NODESIZE	8
#define P_BRANCH	0x01
#define P_LEAF		0x02
#define P_OVERFLOW	0x04
#define P_META		0x08
#define P_LEAF2		0x20
#define P_SUBP		0x40
#define F_BIGDATA	0x01
#define F_SUBDATA	0x02
#define F_DUPDATA	0x04
#define P_INVALID	(~(uint64_t)0)
#define DB_SIZE		48	/* MDB_db */
#define META_SIZE	136	/* MDB_meta */
#define META_MAGIC	0xBEEFC0DEu

static uint16_t rd16(const unsigned char *p) { uint16_t v; memcpy(&v, p, 2); return v; }
static uint32_t rd32(const unsigned char *p) { uint32_t v; memcpy(&v, p, 4); return v; }
static uint64_t rd64(const unsigned char *p) { uint64_t v; memcpy(&v, p, 8); return v; }

/* Page header. mp_lower, mp_upper and mp_ptrs[] are read as stored, that
 * is relative to PAGEBASE. */
#define PAGEHDRSZ	fmt->hdr
#define PAGEBASE	fmt->base
#define EMPTY_LOWER	(PAGEHDRSZ - PAGEBASE)	/* mp_lower of a page without nodes */
#define PG_PGNO(p)	rd64((p) + 0)
#define PG_TXNID(p)	rd64((p) + 8)		/* 1.0 only */
#define PG_PAD(p)	(unsigned)rd16((p) + PAGEHDRSZ - 8)
#define PG_FLAGS(p)	(unsigned)rd16((p) + PAGEHDRSZ - 6)
#define PG_LOWER(p)	(unsigned)rd16((p) + PAGEHDRSZ - 4)
#define PG_UPPER(p)	(unsigned)rd16((p) + PAGEHDRSZ - 2)
#define PG_PAGES(p)	rd32((p) + PAGEHDRSZ - 4)	/* overflow pages: page count */
#define PG_NUMKEYS(p)	((PG_LOWER(p) - EMPTY_LOWER) >> 1)
/* Offset of node i from the page start: mp_ptrs[i] + PAGEBASE. */
#define PG_PTR(p, i)	((size_t)rd16((p) + PAGEHDRSZ + 2 * (i)) + PAGEBASE)
/* NODEDATA(node) - node: the key is padded to an even length in 1.0. */
#define NODE_DATA_OFF(ksize)	(NODESIZE + (fmt->even_keys ? ((ksize) + 1U) & ~1U : (ksize)))

/* MDB_db, decoded */
struct db {
	uint32_t	pad;
	uint16_t	flags, depth;
	uint64_t	branch, leaf, overflow, entries, root;
};

static void
read_db(const unsigned char *p, struct db *db)
{
	db->pad = rd32(p);
	db->flags = rd16(p + 4);
	db->depth = rd16(p + 6);
	db->branch = rd64(p + 8);
	db->leaf = rd64(p + 16);
	db->overflow = rd64(p + 24);
	db->entries = rd64(p + 32);
	db->root = rd64(p + 40);
}

/* MDB_meta, at PAGEHDRSZ in pages 0 and 1 */
struct meta {
	uint32_t	magic, version;
	uint64_t	mapsize;
	struct db	free, main;
	uint64_t	last_pg, txnid;
};

/* The MDB_meta of meta page `page` in the format `l`. */
static void
read_meta(const unsigned char *page, const struct layout *l, struct meta *m)
{
	const unsigned char *p = page + l->hdr;
	m->magic = rd32(p);
	m->version = rd32(p + 4);
	m->mapsize = rd64(p + 16);
	read_db(p + 24, &m->free);
	read_db(p + 24 + DB_SIZE, &m->main);
	m->last_pg = rd64(p + 120);
	m->txnid = rd64(p + 128);
}

/* ---- The walker --------------------------------------------------------- */

/* What a page is used by. Values >= USE_DB + k mean "tree of DB k". */
enum { USE_NONE = 0, USE_META, USE_FREE_LISTED, USE_FREE_DB, USE_DB };

#define MAX_DBS		1024
#define MAX_ERRORS	20

struct walker {
	int		fd;
	unsigned int	psize;
	uint64_t	txnid;		/* the snapshot */
	uint64_t	last_pg;
	uint32_t	*use;		/* per page: USE_* */
	const char	*names[MAX_DBS];	/* DB k's label, for messages */
	int		ndbs;
	unsigned long	errors;
	uint64_t	free_listed;
};

/* Totals found by walking one tree. */
struct counts {
	uint64_t	branch, leaf, overflow, entries;
	uint64_t	sub_branch, sub_leaf;	/* pages of DUPSORT sub-DBs */
	uint64_t	subdbs, subpages;
};

static void
fail(struct walker *w, const char *fmt, ...) __attribute__((format(printf, 2, 3)));

static void
fail(struct walker *w, const char *fmt, ...)
{
	va_list ap;

	if (w->errors++ >= MAX_ERRORS)
		return;
	fputs("cpeer: check-freelist: ", stderr);
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
}

static const char *
use_name(const struct walker *w, uint32_t use, char *buf, size_t len)
{
	switch (use) {
	case USE_META: return "meta";
	case USE_FREE_LISTED: return "free list";
	case USE_FREE_DB: return "free DB tree";
	default:
		snprintf(buf, len, "tree of DB %s", w->names[use - USE_DB]);
		return buf;
	}
}

/* Record that pages pgno..pgno+n-1 are used by `use`. */
static void
mark(struct walker *w, uint64_t pgno, uint64_t n, uint32_t use)
{
	char b1[300], b2[300];
	uint64_t p;

	for (p = pgno; p < pgno + n; p++) {
		if (p > w->last_pg) {
			fail(w, "page %llu (%s) is beyond last_pgno %llu", (unsigned long long)p,
				use_name(w, use, b1, sizeof(b1)), (unsigned long long)w->last_pg);
			return;
		}
		if (w->use[p] != USE_NONE) {
			fail(w, "page %llu used twice: %s and %s", (unsigned long long)p,
				use_name(w, w->use[p], b1, sizeof(b1)), use_name(w, use, b2, sizeof(b2)));
			continue;
		}
		w->use[p] = use;
	}
}

/* Read n pages starting at pgno; NULL (after reporting) on error. */
static unsigned char *
load(struct walker *w, uint64_t pgno, uint64_t n)
{
	size_t len = (size_t)(n * w->psize);
	unsigned char *buf;
	ssize_t got;

	if (pgno < 2 || pgno + n - 1 > w->last_pg) {
		fail(w, "page reference %llu (+%llu) outside 2..%llu", (unsigned long long)pgno,
			(unsigned long long)n, (unsigned long long)w->last_pg);
		return NULL;
	}
	buf = malloc(len);
	got = pread(w->fd, buf, len, (off_t)(pgno * w->psize));
	if (got != (ssize_t)len) {
		fail(w, "short read of page %llu: %zd of %zu bytes", (unsigned long long)pgno,
			got, len);
		free(buf);
		return NULL;
	}
	return buf;
}

/* 1.0: a page reachable from the snapshot was written by a transaction of
 * it, so 1 <= mp_txnid <= snapshot txnid (mdb_page_flush stamps the
 * writer's txnid, the compacting copy 1). C 1.0 writers rely on it: a page
 * with a too large mp_txnid looks like their own and is changed in place. */
static void
check_txnid(struct walker *w, uint64_t pgno, const unsigned char *page)
{
	uint64_t txnid;

	if (!fmt->has_txnid)
		return;
	txnid = PG_TXNID(page);
	if (txnid == 0 || txnid > w->txnid)
		fail(w, "page %llu: mp_txnid %llu is not in 1..%llu (the snapshot)",
			(unsigned long long)pgno, (unsigned long long)txnid,
			(unsigned long long)w->txnid);
}

/* A node of a branch/leaf page; returns 0 if it does not fit in the page. */
struct node {
	uint16_t	flags, ksize;
	uint32_t	dsize;		/* leaf nodes */
	uint64_t	child;		/* branch nodes */
	const unsigned char *key, *data;
};

static int
read_node(const unsigned char *page, size_t page_len, unsigned int i, struct node *n)
{
	size_t off = PG_PTR(page, i);
	const unsigned char *p = page + off;
	uint16_t lo, hi;

	if (off < PAGEHDRSZ || off + NODESIZE > page_len)
		return 0;
	lo = rd16(p);
	hi = rd16(p + 2);
	n->flags = rd16(p + 4);
	n->ksize = rd16(p + 6);
	n->dsize = lo | ((uint32_t)hi << 16);
	n->child = lo | ((uint64_t)hi << 16) | ((uint64_t)n->flags << 32);
	n->key = p + NODESIZE;
	n->data = p + NODE_DATA_OFF(n->ksize);
	return off + NODESIZE + n->ksize <= page_len;
}

/* The bytes the data part of leaf node n takes in its page. */
static size_t
node_data_len(const struct node *n)
{
	return n->flags & F_BIGDATA ? fmt->ovpage : n->dsize;
}

enum tree_kind { TREE_FREE, TREE_MAIN, TREE_NAMED, TREE_DUPSUB };

struct tree {
	struct walker	*w;
	enum tree_kind	kind;
	uint32_t	use;
	unsigned int	depth;		/* expected leaf level (MDB_db.md_depth) */
	unsigned int	dupfixed;	/* the DB has MDB_DUPFIXED */
	struct counts	c;
	/* named DBs found in the main DB, walked afterwards */
	char		**sub_names;
	struct db	*sub_dbs;
	int		nsub;
};

static void walk_db(struct walker *w, const char *label, const struct db *db,
	enum tree_kind kind, uint32_t use, struct counts *out);

/* 1.0: the MDB_ovpage {op_pgno, op_txnid, op_pages} of an F_BIGDATA node
 * against the first page `ov` of its run (op_pgno is its page number). */
static void
check_ovpage(struct walker *w, const struct node *n, const unsigned char *ov)
{
	uint64_t pg = rd64(n->data), op_txnid, op_pages;

	if (!fmt->has_txnid)
		return;
	op_txnid = rd64(n->data + 8);
	op_pages = rd64(n->data + 16);
	if (op_pages != PG_PAGES(ov))
		fail(w, "overflow page %llu: op_pages %llu in the node, mp_pages %u in the page",
			(unsigned long long)pg, (unsigned long long)op_pages, PG_PAGES(ov));
	if (op_txnid == 0 || op_txnid > PG_TXNID(ov))
		fail(w, "overflow page %llu: op_txnid %llu is not in 1..%llu (its mp_txnid)",
			(unsigned long long)pg, (unsigned long long)op_txnid,
			(unsigned long long)PG_TXNID(ov));
}

/* The data of a leaf node, following F_BIGDATA to its overflow pages.
 * Returns a malloc'd buffer the caller frees, or NULL on error. */
static unsigned char *
node_data(struct tree *t, const struct node *n, uint64_t *ovpages)
{
	struct walker *w = t->w;
	unsigned char *buf;

	*ovpages = 0;
	if (n->flags & F_BIGDATA) {
		uint64_t pg = rd64(n->data), npages;
		unsigned char *ov = load(w, pg, 1);
		if (!ov)
			return NULL;
		npages = PG_PAGES(ov);
		if (PG_PGNO(ov) != pg || !(PG_FLAGS(ov) & P_OVERFLOW)) {
			fail(w, "page %llu should be an overflow page (flags 0x%x)",
				(unsigned long long)pg, PG_FLAGS(ov));
			free(ov);
			return NULL;
		}
		check_txnid(w, pg, ov);
		check_ovpage(w, n, ov);
		free(ov);
		if (npages == 0 || (uint64_t)n->dsize + PAGEHDRSZ > npages * w->psize) {
			fail(w, "overflow page %llu: %llu pages for %u bytes",
				(unsigned long long)pg, (unsigned long long)npages, n->dsize);
			return NULL;
		}
		mark(w, pg, npages, t->use);
		*ovpages = npages;
		if (!(ov = load(w, pg, npages)))
			return NULL;
		buf = malloc(n->dsize ? n->dsize : 1);
		memcpy(buf, ov + PAGEHDRSZ, n->dsize);
		free(ov);
		return buf;
	}
	buf = malloc(n->dsize ? n->dsize : 1);
	memcpy(buf, n->data, n->dsize);
	return buf;
}

/* A free DB record: key = txnid, data = [count, pgno...] descending. */
static void
free_record(struct tree *t, const struct node *n, const unsigned char *data)
{
	struct walker *w = t->w;
	uint64_t txnid, count, k, prev = P_INVALID;

	if (n->ksize != 8) {
		fail(w, "free DB key of %u bytes", n->ksize);
		return;
	}
	txnid = rd64(n->key);
	if (n->dsize < 8 || (count = rd64(data), (count + 1) * 8 > n->dsize)) {
		fail(w, "free DB record %llu: %u bytes do not hold its page list",
			(unsigned long long)txnid, n->dsize);
		return;
	}
	for (k = 0; k < count; k++) {
		uint64_t pg = rd64(data + 8 * (k + 1));
		if (pg >= prev)
			fail(w, "free DB record %llu is not sorted descending at %llu",
				(unsigned long long)txnid, (unsigned long long)pg);
		prev = pg;
		mark(w, pg, 1, USE_FREE_LISTED);
	}
	w->free_listed += count;
}

/* A leaf node of a main/named/sub DB. */
static void
leaf_node(struct tree *t, const struct node *n)
{
	struct walker *w = t->w;
	unsigned int f = n->flags & (F_BIGDATA | F_SUBDATA | F_DUPDATA);

	if (f == F_BIGDATA) {
		uint64_t ovpages;
		free(node_data(t, n, &ovpages));
		t->c.overflow += ovpages;
		t->c.entries++;
	} else if (f == (F_SUBDATA | F_DUPDATA)) {
		/* DUPSORT values in a sub-DB of their own */
		struct db sub;
		struct counts sc;
		if (n->dsize != DB_SIZE) {
			fail(w, "sub-DB record of %u bytes", n->dsize);
			return;
		}
		read_db(n->data, &sub);
		walk_db(w, w->names[t->use - USE_DB], &sub, TREE_DUPSUB, t->use, &sc);
		t->c.sub_branch += sc.branch;
		t->c.sub_leaf += sc.leaf;
		t->c.entries += sc.entries;
		t->c.subdbs++;
	} else if (f == F_DUPDATA) {
		/* DUPSORT values in a sub-page inside the node: P_LEAF|P_SUBP
		 * (|P_LEAF2), and in 0.9 maybe P_DIRTY, which C 0.9 sets */
		const unsigned char *sp = n->data;
		unsigned int want = P_LEAF | P_SUBP | (t->dupfixed ? P_LEAF2 : 0);
		if (n->dsize < PAGEHDRSZ || (PG_FLAGS(sp) & ~(unsigned)fmt->subp_dirty) != want ||
			PG_LOWER(sp) < EMPTY_LOWER || PG_LOWER(sp) + PAGEBASE > n->dsize) {
			fail(w, "bad sub-page in DB %s (flags 0x%x, %u bytes)",
				w->names[t->use - USE_DB], PG_FLAGS(sp), n->dsize);
			return;
		}
		t->c.entries += PG_NUMKEYS(sp);
		t->c.subpages++;
	} else if (f == F_SUBDATA) {
		/* a named DB, only valid in the main DB */
		if (t->kind != TREE_MAIN || n->dsize != DB_SIZE) {
			fail(w, "unexpected named-DB record in DB %s", w->names[t->use - USE_DB]);
			return;
		}
		t->sub_names = realloc(t->sub_names, sizeof(char *) * (t->nsub + 1));
		t->sub_dbs = realloc(t->sub_dbs, sizeof(struct db) * (t->nsub + 1));
		t->sub_names[t->nsub] = malloc(n->ksize + 1);
		memcpy(t->sub_names[t->nsub], n->key, n->ksize);
		t->sub_names[t->nsub][n->ksize] = '\0';
		read_db(n->data, &t->sub_dbs[t->nsub]);
		t->nsub++;
		t->c.entries++;
	} else if (f == 0) {
		if (t->kind == TREE_DUPSUB && n->dsize != 0)
			fail(w, "sub-DB node with %u data bytes", n->dsize);
		t->c.entries++;
	} else {
		fail(w, "bad node flags 0x%x in DB %s", n->flags, w->names[t->use - USE_DB]);
	}
}

static void
walk_page(struct tree *t, uint64_t pgno, unsigned int level)
{
	struct walker *w = t->w;
	unsigned char *page = load(w, pgno, 1);
	unsigned int i, nkeys;
	uint16_t flags;

	if (!page)
		return;
	flags = PG_FLAGS(page);
	if (PG_PGNO(page) != pgno) {
		fail(w, "page %llu says it is page %llu", (unsigned long long)pgno,
			(unsigned long long)PG_PGNO(page));
		goto out;
	}
	check_txnid(w, pgno, page);
	if (PG_LOWER(page) < EMPTY_LOWER || PG_LOWER(page) > PG_UPPER(page) ||
		PG_UPPER(page) + PAGEBASE > w->psize) {
		fail(w, "page %llu: bad lower/upper %u/%u", (unsigned long long)pgno,
			PG_LOWER(page), PG_UPPER(page));
		goto out;
	}
	mark(w, pgno, 1, t->use);
	nkeys = PG_NUMKEYS(page);

	if (flags & P_BRANCH) {
		t->c.branch++;
		if (level >= t->depth)
			fail(w, "branch page %llu at level %u of a tree of depth %u",
				(unsigned long long)pgno, level, t->depth);
		for (i = 0; i < nkeys; i++) {
			struct node n;
			if (!read_node(page, w->psize, i, &n)) {
				fail(w, "branch page %llu: node %u out of bounds", (unsigned long long)pgno, i);
				break;
			}
			walk_page(t, n.child, level + 1);
		}
	} else if (flags & P_LEAF) {
		t->c.leaf++;
		if (level != t->depth)
			fail(w, "leaf page %llu at level %u of a tree of depth %u",
				(unsigned long long)pgno, level, t->depth);
		if (flags & P_LEAF2) {
			/* DUPFIXED sub-DB: packed values, size in mp_pad */
			if (t->kind != TREE_DUPSUB || !PG_PAD(page))
				fail(w, "unexpected LEAF2 page %llu", (unsigned long long)pgno);
			t->c.entries += nkeys;
			goto out;
		}
		for (i = 0; i < nkeys; i++) {
			struct node n;
			if (!read_node(page, w->psize, i, &n) ||
				n.data + node_data_len(&n) > page + w->psize) {
				fail(w, "leaf page %llu: node %u out of bounds", (unsigned long long)pgno, i);
				break;
			}
			if (t->kind == TREE_FREE) {
				uint64_t ovpages;
				unsigned char *data = node_data(t, &n, &ovpages);
				t->c.overflow += ovpages;
				t->c.entries++;
				if (data)
					free_record(t, &n, data);
				free(data);
			} else {
				leaf_node(t, &n);
			}
		}
	} else {
		fail(w, "page %llu has unexpected flags 0x%x", (unsigned long long)pgno, flags);
	}
out:
	free(page);
}

/* Walk a whole tree and compare the totals with its MDB_db. */
static void
walk_db(struct walker *w, const char *label, const struct db *db, enum tree_kind kind,
	uint32_t use, struct counts *out)
{
	struct tree t;
	uint64_t want_branch, want_leaf;
	int k;

	memset(&t, 0, sizeof(t));
	t.w = w;
	t.kind = kind;
	t.use = use;
	t.depth = db->depth;
	t.dupfixed = (db->flags & MDB_DUPFIXED) != 0;
	if (db->root == P_INVALID) {
		if (db->depth || db->branch || db->leaf || db->overflow || db->entries)
			fail(w, "DB %s is empty but its descriptor has depth %u, entries %llu",
				label, db->depth, (unsigned long long)db->entries);
	} else {
		walk_page(&t, db->root, 1);
	}

	/* Since ITS#9388 a DUPSORT DB's branch/leaf counts include the pages
	 * of its sub-DBs; overflow pages never occur in sub-DBs. */
	want_branch = t.c.branch + t.c.sub_branch;
	want_leaf = t.c.leaf + t.c.sub_leaf;
	if (db->branch != want_branch || db->leaf != want_leaf ||
		db->overflow != t.c.overflow || db->entries != t.c.entries)
		fail(w, "DB %s%s: descriptor says branch=%llu leaf=%llu overflow=%llu entries=%llu,"
			" walk found branch=%llu leaf=%llu overflow=%llu entries=%llu",
			label, kind == TREE_DUPSUB ? " (a sub-DB)" : "",
			(unsigned long long)db->branch, (unsigned long long)db->leaf,
			(unsigned long long)db->overflow, (unsigned long long)db->entries,
			(unsigned long long)want_branch, (unsigned long long)want_leaf,
			(unsigned long long)t.c.overflow, (unsigned long long)t.c.entries);
	if (kind != TREE_DUPSUB)
		printf("db=%s flags=0x%x depth=%u branch_pages=%llu leaf_pages=%llu"
			" overflow_pages=%llu entries=%llu subdbs=%llu subpages=%llu\n",
			label, db->flags, db->depth, (unsigned long long)want_branch,
			(unsigned long long)want_leaf, (unsigned long long)t.c.overflow,
			(unsigned long long)t.c.entries, (unsigned long long)t.c.subdbs,
			(unsigned long long)t.c.subpages);
	if (out)
		*out = t.c;

	/* Named DBs recorded in the main DB are trees of their own. */
	for (k = 0; k < t.nsub; k++) {
		if (w->ndbs == MAX_DBS) {
			fail(w, "too many named DBs");
			break;
		}
		w->names[w->ndbs] = t.sub_names[k];
		walk_db(w, t.sub_names[k], &t.sub_dbs[k], TREE_NAMED, USE_DB + w->ndbs++, NULL);
	}
	free(t.sub_names);	/* the strings live on in w->names */
	free(t.sub_dbs);
}

/* Stop with a clear message: meta page k (its first bytes in `buf`) is not
 * a meta page of the format this program decodes. */
static void wrong_format(int k, const unsigned char *buf) __attribute__((noreturn));

static void
wrong_format(int k, const unsigned char *buf)
{
	struct meta m, o;

	read_meta(buf, fmt, &m);
	read_meta(buf, other_fmt, &o);
	if (o.magic == META_MAGIC && o.version == other_fmt->version)
		die(EXIT_MISMATCH, "check-freelist: meta page %d is in the %s format (data version %u),"
			" but this program decodes %s files (data version %u)", k, other_fmt->name,
			o.version, fmt->name, fmt->version);
	die(EXIT_MISMATCH, "check-freelist: meta page %d: magic 0x%x version %u at offset %u,"
		" not an %s meta page (magic 0x%x version %u)", k, m.magic, m.version, fmt->hdr,
		fmt->name, META_MAGIC, fmt->version);
}

#ifdef LMDB_RUST
/* rlmdb opens environments of both formats: decode the one whose meta page
 * is at its place in page 0 (`buf`). */
static void
choose_format(const unsigned char *buf)
{
	struct meta m;
	int k;

	for (k = 0; k < 2; k++) {
		read_meta(buf, &layouts[k], &m);
		if (m.magic == META_MAGIC && m.version == layouts[k].version) {
			fmt = &layouts[k];
			other_fmt = &layouts[1 - k];
			return;
		}
	}
}
#endif

/* Read both meta pages and return the one of snapshot `txnid`. */
static int
pick_meta(int fd, uint64_t txnid, struct meta *out, unsigned int *psize)
{
	unsigned char buf[MAX_PAGEHDRSZ + META_SIZE];
	struct meta m;
	int k;

	for (k = 0; k < 2; k++) {
		off_t off = k == 0 ? 0 : (off_t)*psize;
		if (pread(fd, buf, sizeof(buf), off) != (ssize_t)sizeof(buf))
			die(EXIT_LMDB, "check-freelist: cannot read meta page %d", k);
#ifdef LMDB_RUST
		if (k == 0)
			choose_format(buf);
#endif
		read_meta(buf, fmt, &m);
		if (m.magic != META_MAGIC || m.version != fmt->version)
			wrong_format(k, buf);
		if (PG_PGNO(buf) != (uint64_t)k || !(PG_FLAGS(buf) & P_META))
			die(EXIT_MISMATCH, "check-freelist: page %d is not a meta page", k);
		if (k == 0)
			*psize = m.free.pad;	/* mm_psize */
		if (m.txnid == txnid) {
			*out = m;
			return 1;
		}
	}
	return 0;
}

int
cmd_check_freelist(struct opts *o)
{
	struct walker w;
	struct meta m;
	struct counts fc;
	MDB_env *env;
	MDB_txn *txn;
	MDB_stat st;
	const char *path;
	char fname[4096];
	uint64_t txnid, p, leaks = 0, tree = 0;
	int tries;

	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_env_get_path(env, &path));
	snprintf(fname, sizeof(fname), o->nosubdir ? "%s" : "%s/data.mdb", path);
	memset(&w, 0, sizeof(w));
	if ((w.fd = open(fname, O_RDONLY)) < 0)
		die(EXIT_LMDB, "open %s: %s", fname, strerror(errno));
	CHECK(mdb_env_stat(env, &st));
	w.psize = st.ms_psize;

	/* Pin a snapshot, then find its meta page. A concurrent writer may
	 * already have overwritten it (two commits later); then try again. */
	for (tries = 0;; tries++) {
		CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
		txnid = mdb_txn_id(txn);
		if (pick_meta(w.fd, txnid, &m, &w.psize))
			break;
		mdb_txn_abort(txn);
		if (tries == 100)
			die(EXIT_MISMATCH, "check-freelist: no meta page has txnid %llu",
				(unsigned long long)txnid);
	}
	if (w.psize != st.ms_psize)
		fail(&w, "meta psize %u, mdb_env_stat says %u", w.psize, st.ms_psize);

	w.txnid = txnid;
	w.last_pg = m.last_pg;
	w.use = calloc(m.last_pg + 1, sizeof(*w.use));
	w.use[0] = w.use[1] = USE_META;
	printf("psize=%u\ntxnid=%llu\nlast_pgno=%llu\n", w.psize, (unsigned long long)txnid,
		(unsigned long long)m.last_pg);

	w.names[0] = "-";
	w.ndbs = 1;
	walk_db(&w, "@free", &m.free, TREE_FREE, USE_FREE_DB, &fc);
	walk_db(&w, "-", &m.main, TREE_MAIN, USE_DB + 0, NULL);
	mdb_txn_abort(txn);

	for (p = 0; p <= m.last_pg; p++) {
		if (w.use[p] == USE_NONE) {
			if (leaks++ < MAX_ERRORS)
				fail(&w, "page %llu is leaked (neither used nor free)",
					(unsigned long long)p);
		} else if (w.use[p] >= USE_DB) {
			tree++;
		}
	}
	printf("pages_meta=2\npages_freedb=%llu\npages_free=%llu\npages_tree=%llu\n"
		"pages_leaked=%llu\npages_total=%llu\n",
		(unsigned long long)(fc.branch + fc.leaf + fc.overflow),
		(unsigned long long)w.free_listed, (unsigned long long)tree,
		(unsigned long long)leaks, (unsigned long long)(m.last_pg + 1));
	for (p = 1; p < (uint64_t)w.ndbs; p++)
		free((char *)w.names[p]);
	free(w.use);
	close(w.fd);
	mdb_env_close(env);
	if (w.errors) {
		printf("errors=%lu\n", w.errors);
		return EXIT_MISMATCH;
	}
	printf("ok\n");
	return 0;
}
