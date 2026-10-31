/* dataset.c - the deterministic data set shared by fill, verify and delete.
 *
 * Everything here is a pure function of (seed, i, d) so that any peer, in
 * any process, can regenerate the exact bytes another peer wrote. The
 * definitions are documented in README.md ("Deterministic data set"); keep
 * the two in sync, the Rust peer implements the README.
 */
#include <stdlib.h>
#include <string.h>

#include "cpeer.h"

/* The 32-bit finaliser of MurmurHash3. A bijection on uint32_t, so distinct
 * inputs give distinct outputs. */
uint32_t
fmix32(uint32_t h)
{
	h ^= h >> 16;
	h *= 0x85ebca6bu;
	h ^= h >> 13;
	h *= 0xc2b2ae35u;
	h ^= h >> 16;
	return h;
}

/* SplitMix64: a tiny PRNG; each call advances *state and returns 64 bits. */
uint64_t
splitmix64(uint64_t *state)
{
	uint64_t z = (*state += 0x9e3779b97f4a7c15ull);
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ull;
	z = (z ^ (z >> 27)) * 0x94d049bb133111ebull;
	return z ^ (z >> 31);
}

const char *
ds_finish(struct dataset *ds)
{
	unsigned int f = ds->dbflags;

	if (f & MDB_INTEGERDUP)
		f |= MDB_DUPSORT | MDB_DUPFIXED;
	if (f & MDB_DUPFIXED)
		f |= MDB_DUPSORT;
	ds->dbflags = f;

	if (!ds->valsize) {
		if (f & MDB_INTEGERDUP)
			ds->valsize = 8;
		else if (f & MDB_DUPSORT)
			ds->valsize = 16;
		else
			ds->valsize = 32;
	}
	if (!ds->dups)
		ds->dups = (f & MDB_DUPSORT) ? 5 : 1;

	if ((f & MDB_INTEGERDUP) && ds->valsize != 4 && ds->valsize != 8)
		return "--intdup needs --valsize 4 or 8";
	if ((f & MDB_DUPFIXED) && (ds->valsize < 1 || ds->valsize > 511))
		return "DUPFIXED values must be 1..511 bytes (--valsize)";
	if ((f & MDB_DUPFIXED) && ds->valsize < 4 && ds->dups > 1u << (8 * ds->valsize))
		return "--dups exceeds the distinct values of --valsize";
	if ((f & MDB_DUPSORT) && !(f & MDB_DUPFIXED) && (ds->valsize < 4 || ds->valsize > 511))
		return "DUPSORT values must be 4..511 bytes (--valsize)";
	if ((f & MDB_DUPSORT) && ds->big)
		return "--big cannot be used with DUPSORT (dup data is limited to 511 bytes)";
	if (!(f & MDB_DUPSORT) && ds->dups > 1)
		return "--dups needs --dupsort, --dupfixed or --intdup";
	if ((f & MDB_INTEGERKEY) && (f & MDB_REVERSEKEY))
		return "--intkey and --revkey are exclusive";
	return NULL;
}

/* Per-key hash; the bijection makes keys unique for i < 2^32. */
static uint32_t
key_hash(const struct dataset *ds, uint32_t i)
{
	return fmix32(i + ds->seed * 0x9e3779b9u);
}

size_t
ds_key(const struct dataset *ds, uint32_t i, unsigned char *buf)
{
	if (ds->dbflags & MDB_INTEGERKEY) {
		/* A native-endian size_t: hash in the high half, i in the low half. */
		uint64_t k = ((uint64_t)key_hash(ds, i) << 32) | i;
		memcpy(buf, &k, sizeof(k));
		return sizeof(k);
	}
	/* "%08x.%u": 10 to 19 bytes, sorts by the hash, decodes back to i. */
	return (size_t)snprintf((char *)buf, DS_MAX_KEY, "%08x.%u", key_hash(ds, i), i);
}

uint32_t
ds_ndups(const struct dataset *ds, uint32_t i)
{
	if (!(ds->dbflags & MDB_DUPSORT) || ds->dups <= 1)
		return 1;
	return 1 + fmix32(i ^ (ds->seed * 0x85ebca6bu)) % ds->dups;
}

/* Distinct for distinct d (XOR with d is a bijection), so the values of one
 * key never collide; it is also the sort prefix of the value. */
static uint32_t
value_tag(const struct dataset *ds, uint32_t i, uint32_t d)
{
	return fmix32(i * 0x9e3779b9u + ds->seed) ^ d;
}

size_t
ds_vlen(const struct dataset *ds, uint32_t i, uint32_t d)
{
	unsigned int f = ds->dbflags;

	if (f & (MDB_INTEGERDUP | MDB_DUPFIXED))
		return ds->valsize;
	if (f & MDB_DUPSORT)
		return 4 + fmix32(value_tag(ds, i, d)) % (ds->valsize - 3);
	if (ds->big && i % 3 == 0)
		return ds->big;
	return ds->valsize;
}

void
ds_value(const struct dataset *ds, uint32_t i, uint32_t d,
	unsigned char *buf, size_t len)
{
	uint32_t tag = value_tag(ds, i, d);
	uint64_t state;
	size_t off;

	if (ds->dbflags & MDB_INTEGERDUP) {
		if (len == 8) {
			uint64_t v = ((uint64_t)tag << 32) | d;
			memcpy(buf, &v, 8);
		} else {
			memcpy(buf, &tag, 4);
		}
		return;
	}
	/* DUPFIXED values under 4 bytes: the low bytes of the tag, big-endian
	 * (distinct, as d only changes them: --dups is limited to fit). */
	if ((ds->dbflags & MDB_DUPFIXED) && len < 4) {
		for (off = 0; off < len; off++)
			buf[off] = (unsigned char)(tag >> (8 * (len - 1 - off)));
		return;
	}
	/* 4-byte big-endian tag, then a SplitMix64 stream (little-endian words). */
	for (off = 0; off < 4 && off < len; off++)
		buf[off] = (unsigned char)(tag >> (24 - 8 * off));
	state = (uint64_t)ds->seed * 0x9e3779b97f4a7c15ull ^ ((uint64_t)i << 16) ^ d;
	while (off < len) {
		uint64_t r = splitmix64(&state);
		size_t b;
		for (b = 0; b < 8 && off < len; b++, off++)
			buf[off] = (unsigned char)(r >> (8 * b));
	}
}

int
ds_delete_key(uint32_t every, uint32_t i)
{
	return every && i % every == 0;
}

int
ds_delete_dup(const struct dataset *ds, uint32_t every, uint32_t i, uint32_t d)
{
	return every > 1 && (ds->dbflags & MDB_DUPSORT) && i % every == 1 && d % 2 == 0;
}

int
ds_present(const struct dataset *ds, uint32_t i, uint32_t d)
{
	uint32_t every = ds->deleted_every;
	return !ds_delete_key(every, i) && !ds_delete_dup(ds, every, i, d);
}

/* ---- Comparators: equivalents of mdb.c's, to sort the expected model ---- */

/* mdb_cmp_memn: memcmp, then shorter first */
static int
cmp_memn(const MDB_val *a, const MDB_val *b)
{
	size_t len = a->mv_size < b->mv_size ? a->mv_size : b->mv_size;
	int diff = memcmp(a->mv_data, b->mv_data, len);
	if (diff)
		return diff;
	return a->mv_size < b->mv_size ? -1 : a->mv_size > b->mv_size;
}

/* mdb_cmp_memnr: compare bytes from the end, then shorter first */
static int
cmp_memnr(const MDB_val *a, const MDB_val *b)
{
	const unsigned char *p1 = (const unsigned char *)a->mv_data + a->mv_size;
	const unsigned char *p2 = (const unsigned char *)b->mv_data + b->mv_size;
	size_t len = a->mv_size < b->mv_size ? a->mv_size : b->mv_size;

	while (len--) {
		int diff = *--p1 - *--p2;
		if (diff)
			return diff;
	}
	return a->mv_size < b->mv_size ? -1 : a->mv_size > b->mv_size;
}

/* mdb_cmp_int / mdb_cmp_cint / mdb_cmp_long: native unsigned integers of
 * 4 or 8 bytes (both sides have the same size in our data sets). */
static int
cmp_uint(const MDB_val *a, const MDB_val *b)
{
	if (a->mv_size == 8) {
		uint64_t x, y;
		memcpy(&x, a->mv_data, 8);
		memcpy(&y, b->mv_data, 8);
		return x < y ? -1 : x > y;
	} else {
		uint32_t x, y;
		memcpy(&x, a->mv_data, 4);
		memcpy(&y, b->mv_data, 4);
		return x < y ? -1 : x > y;
	}
}

/* As mdb_default_cmp(): key comparator */
MDB_cmp_func *
ds_key_cmp(unsigned int f)
{
	return (f & MDB_REVERSEKEY) ? cmp_memnr : (f & MDB_INTEGERKEY) ? cmp_uint : cmp_memn;
}

/* As mdb_default_cmp(): data comparator of DUPSORT DBs */
MDB_cmp_func *
ds_dup_cmp(unsigned int f)
{
	return (f & MDB_INTEGERDUP) ? cmp_uint : (f & MDB_REVERSEDUP) ? cmp_memnr : cmp_memn;
}

/* ---- The expected model ------------------------------------------------- */

/* qsort() has no context argument; the model is built single-threaded. */
static MDB_cmp_func *sort_cmp;

static int
sort_items(const void *a, const void *b)
{
	return sort_cmp(&((const struct model_item *)a)->key,
		&((const struct model_item *)b)->key);
}

static int
sort_vals(const void *a, const void *b)
{
	return sort_cmp((const MDB_val *)a, (const MDB_val *)b);
}

static void *
xmalloc(size_t n)
{
	void *p = malloc(n ? n : 1);
	if (!p)
		die(EXIT_LMDB, "out of memory");
	return p;
}

int
model_build(const struct dataset *ds, struct model *m)
{
	uint32_t i, d;

	memset(m, 0, sizeof(*m));
	m->items = xmalloc(sizeof(*m->items) * ds->count);
	m->absent = xmalloc(sizeof(*m->absent) * ds->count);
	for (i = 0; i < ds->count; i++) {
		struct model_item *it = &m->items[m->nitems];
		uint32_t nd = ds_ndups(ds, i);
		unsigned char kbuf[DS_MAX_KEY];

		it->i = i;
		it->key.mv_size = ds_key(ds, i, kbuf);
		it->key.mv_data = xmalloc(it->key.mv_size);
		memcpy(it->key.mv_data, kbuf, it->key.mv_size);
		it->vals = xmalloc(sizeof(MDB_val) * nd);
		it->nvals = 0;
		for (d = 0; d < nd; d++) {
			MDB_val *v;
			if (!ds_present(ds, i, d))
				continue;
			v = &it->vals[it->nvals++];
			v->mv_size = ds_vlen(ds, i, d);
			v->mv_data = xmalloc(v->mv_size);
			ds_value(ds, i, d, v->mv_data, v->mv_size);
		}
		if (it->nvals == 0) {
			free(it->key.mv_data);
			free(it->vals);
			m->absent[m->nabsent++] = i;
			continue;
		}
		if (it->nvals > 1) {
			sort_cmp = ds_dup_cmp(ds->dbflags);
			qsort(it->vals, it->nvals, sizeof(MDB_val), sort_vals);
		}
		m->nentries += it->nvals;
		m->nitems++;
	}
	sort_cmp = ds_key_cmp(ds->dbflags);
	qsort(m->items, m->nitems, sizeof(*m->items), sort_items);
	return 0;
}

void
model_free(struct model *m)
{
	size_t k, j;

	for (k = 0; k < m->nitems; k++) {
		for (j = 0; j < m->items[k].nvals; j++)
			free(m->items[k].vals[j].mv_data);
		free(m->items[k].vals);
		free(m->items[k].key.mv_data);
	}
	free(m->items);
	free(m->absent);
	memset(m, 0, sizeof(*m));
}
