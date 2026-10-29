/* cpeer_data.c - fill, verify, delete and dump of the deterministic data set,
 * and drop of a whole DB.
 *
 * fill and delete write; verify proves that a DB holds exactly the data set
 * (as modified by a prior delete), using every read path a peer must get
 * right: point lookups, full cursor walks in both directions, duplicate
 * navigation, MDB_GET_MULTIPLE for DUPFIXED, and the DB statistics.
 */
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>

#include "cpeer.h"

/* Largest value the data set produces. */
static size_t
max_value_size(const struct dataset *ds)
{
	return ds->big > ds->valsize ? ds->big : ds->valsize;
}

static void
print_db_stat(const char *what, MDB_txn *txn, MDB_dbi dbi, const struct dataset *ds,
	unsigned int txns)
{
	MDB_stat st;

	CHECK(mdb_stat(txn, dbi, &st));
	printf("%s db=%s keys=%u entries=%zu txns=%u psize=%u depth=%u"
		" branch_pages=%zu leaf_pages=%zu overflow_pages=%zu\n",
		what, db_label(ds->db), ds->count, st.ms_entries, txns, st.ms_psize,
		st.ms_depth, st.ms_branch_pages, st.ms_leaf_pages, st.ms_overflow_pages);
}

/* ---- fill --------------------------------------------------------------- */

/* Put one item with mdb_put (optionally MDB_RESERVE) */
static void
put_item(MDB_txn *txn, MDB_dbi dbi, const struct opts *o, uint32_t i, uint32_t d,
	MDB_val *key, unsigned char *vbuf)
{
	const struct dataset *ds = &o->ds;
	MDB_val data;

	data.mv_size = ds_vlen(ds, i, d);
	if (o->reserve) {
		/* MDB_RESERVE: LMDB hands out the space, we write the value into it. */
		CHECK(mdb_put(txn, dbi, key, &data, MDB_RESERVE));
		ds_value(ds, i, d, data.mv_data, data.mv_size);
	} else {
		ds_value(ds, i, d, vbuf, data.mv_size);
		data.mv_data = vbuf;
		CHECK(mdb_put(txn, dbi, key, &data, 0));
	}
}

int
cmd_fill(struct opts *o)
{
	struct dataset *ds = &o->ds;
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	unsigned char *vbuf;
	unsigned int txns = 1;
	uint32_t n = 0;

	parse_dataset_args(o, 1);
	if (o->reserve && (ds->dbflags & MDB_DUPSORT))
		die(EXIT_USAGE, "--reserve cannot be used with DUPSORT");
	env = open_env(o, 0);
	vbuf = malloc(max_value_size(ds));
	txn = begin_write(env);
	CHECK(mdb_dbi_open(txn, ds->db, ds->dbflags | MDB_CREATE, &dbi));

	if (o->append) {
		/* Insert in comparator order with MDB_APPEND (new key) and
		 * MDB_APPENDDUP (further values of the same key) via a cursor. The
		 * DB must not already hold keys sorting after ours. */
		struct model m;
		MDB_cursor *cur;
		size_t k, j;

		model_build(ds, &m);
		CHECK(mdb_cursor_open(txn, dbi, &cur));
		for (k = 0; k < m.nitems; k++) {
			struct model_item *it = &m.items[k];
			for (j = 0; j < it->nvals; j++)
				CHECK(mdb_cursor_put(cur, &it->key, &it->vals[j],
					j == 0 ? MDB_APPEND : MDB_APPENDDUP));
			if (o->per_txn && ++n % o->per_txn == 0 && k + 1 < m.nitems) {
				mdb_cursor_close(cur);
				commit_write(txn);
				txn = begin_write(env);
				txns++;
				CHECK(mdb_cursor_open(txn, dbi, &cur));
			}
		}
		mdb_cursor_close(cur);
		model_free(&m);
	} else {
		/* Insert in index order, which is random order for the comparator. */
		uint32_t i, d;
		for (i = 0; i < ds->count; i++) {
			unsigned char kbuf[DS_MAX_KEY];
			MDB_val key;
			key.mv_data = kbuf;
			key.mv_size = ds_key(ds, i, kbuf);
			for (d = 0; d < ds_ndups(ds, i); d++)
				if (ds_present(ds, i, d))
					put_item(txn, dbi, o, i, d, &key, vbuf);
			if (o->per_txn && ++n % o->per_txn == 0 && i + 1 < ds->count) {
				commit_write(txn);
				txn = begin_write(env);
				txns++;
			}
		}
	}
	print_db_stat("filled", txn, dbi, ds, txns);
	commit_write(txn);
	free(vbuf);
	mdb_env_close(env);
	return 0;
}

/* ---- verify ------------------------------------------------------------- */

static int
mismatch(const char *db, const char *fmt, ...)
	__attribute__((format(printf, 2, 3)));

static int
mismatch(const char *db, const char *fmt, ...)
{
	va_list ap;

	fflush(stdout);
	fprintf(stderr, "cpeer: verify db=%s: ", db_label(db));
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
	fputc('\n', stderr);
	return EXIT_MISMATCH;
}

static int
val_eq(const MDB_val *a, const MDB_val *b)
{
	return a->mv_size == b->mv_size && !memcmp(a->mv_data, b->mv_data, a->mv_size);
}

/* Print up to 40 bytes of a value as hex, for error messages. */
static const char *
short_hex(const MDB_val *v, char *buf)
{
	size_t n = v->mv_size > 40 ? 40 : v->mv_size;
	hex_encode(v->mv_data, n, buf);
	if (n < v->mv_size)
		strcat(buf, "...");
	return buf;
}

/* Walk the whole DB with `step` (MDB_NEXT or MDB_PREV) and compare it with
 * the flattened model: entry (k, j) is value j of model item k. */
static int
check_walk(MDB_cursor *cur, const struct dataset *ds, const struct model *m,
	MDB_cursor_op first, MDB_cursor_op step)
{
	const char *dir = step == MDB_NEXT ? "forward" : "backward";
	char h1[100], h2[100], h3[100], h4[100];
	MDB_val key, data;
	size_t seen = 0, k = 0, j = 0;
	int rc;

	if (step == MDB_PREV && m->nitems) {
		k = m->nitems - 1;
		j = m->items[k].nvals - 1;
	}
	for (rc = mdb_cursor_get(cur, &key, &data, first); rc == 0;
		rc = mdb_cursor_get(cur, &key, &data, step)) {
		const struct model_item *it;

		if (seen++ >= m->nentries)
			return mismatch(ds->db, "%s walk: extra item key=%s", dir,
				short_hex(&key, h1));
		it = &m->items[k];
		if (!val_eq(&key, &it->key) || !val_eq(&data, &it->vals[j]))
			return mismatch(ds->db, "%s walk: entry %zu is %s=%s, expected %s=%s (i=%u)",
				dir, seen - 1, short_hex(&key, h1), short_hex(&data, h2),
				short_hex(&it->key, h3), short_hex(&it->vals[j], h4), it->i);
		/* advance (k, j) in walk direction */
		if (step == MDB_NEXT) {
			if (++j == it->nvals) {
				k++;
				j = 0;
			}
		} else if (j > 0) {
			j--;
		} else if (k > 0) {
			k--;
			j = m->items[k].nvals - 1;
		}
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get(walk)");
	if (seen != m->nentries)
		return mismatch(ds->db, "%s walk saw %zu entries, expected %zu", dir, seen,
			m->nentries);
	return 0;
}

/* Duplicate navigation: count, GET_BOTH, NEXT_NODUP, FIRST_DUP/LAST_DUP and,
 * for DUPFIXED, GET_MULTIPLE/NEXT_MULTIPLE. */
static int
check_dups(MDB_cursor *cur, const struct dataset *ds, const struct model *m)
{
	char h1[100];
	MDB_val key, data;
	size_t k, j, nkeys = 0, count;
	int rc;

	for (k = 0; k < m->nitems; k++) {
		const struct model_item *it = &m->items[k];

		key = it->key;
		CHECK(mdb_cursor_get(cur, &key, &data, MDB_SET));
		CHECK(mdb_cursor_count(cur, &count));
		if (count != it->nvals)
			return mismatch(ds->db, "key i=%u has %zu values, expected %u",
				it->i, (size_t)count, it->nvals);
		CHECK(mdb_cursor_get(cur, &key, &data, MDB_LAST_DUP));
		if (!val_eq(&data, &it->vals[it->nvals - 1]))
			return mismatch(ds->db, "MDB_LAST_DUP of i=%u is %s", it->i, short_hex(&data, h1));
		CHECK(mdb_cursor_get(cur, &key, &data, MDB_FIRST_DUP));
		if (!val_eq(&data, &it->vals[0]))
			return mismatch(ds->db, "MDB_FIRST_DUP of i=%u is %s", it->i, short_hex(&data, h1));
		for (j = 0; j < it->nvals; j++) {
			MDB_val k2 = it->key, d2 = it->vals[j];
			rc = mdb_cursor_get(cur, &k2, &d2, MDB_GET_BOTH);
			if (rc == MDB_NOTFOUND)
				return mismatch(ds->db, "MDB_GET_BOTH: value %zu of i=%u not found", j, it->i);
			check_rc(rc, "mdb_cursor_get(MDB_GET_BOTH)");
			if (!val_eq(&d2, &it->vals[j]))
				return mismatch(ds->db, "MDB_GET_BOTH of i=%u returned %s",
					it->i, short_hex(&d2, h1));
		}
		if (ds->dbflags & MDB_DUPFIXED) {
			/* GET_MULTIPLE returns up to a page of packed values, and
			 * NEXT_MULTIPLE the following pages, of the current key. */
			size_t got = 0;
			CHECK(mdb_cursor_get(cur, &key, &data, MDB_SET));
			for (rc = mdb_cursor_get(cur, &key, &data, MDB_GET_MULTIPLE); rc == 0;
				rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT_MULTIPLE)) {
				size_t off;
				if (data.mv_size % ds->valsize)
					return mismatch(ds->db, "GET_MULTIPLE size %zu not a multiple of %u",
						data.mv_size, ds->valsize);
				for (off = 0; off < data.mv_size; off += ds->valsize, got++) {
					if (got >= it->nvals || memcmp((char *)data.mv_data + off,
						it->vals[got].mv_data, ds->valsize))
						return mismatch(ds->db, "GET_MULTIPLE value %zu of i=%u differs",
							got, it->i);
				}
			}
			if (rc != MDB_NOTFOUND)
				check_rc(rc, "mdb_cursor_get(MDB_NEXT_MULTIPLE)");
			if (got != it->nvals)
				return mismatch(ds->db, "GET_MULTIPLE saw %zu values of i=%u, expected %u",
					got, it->i, it->nvals);
		}
	}
	for (rc = mdb_cursor_get(cur, &key, &data, MDB_FIRST); rc == 0;
		rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT_NODUP))
		nkeys++;
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get(MDB_NEXT_NODUP)");
	if (nkeys != m->nitems)
		return mismatch(ds->db, "MDB_NEXT_NODUP saw %zu keys, expected %zu", nkeys, m->nitems);
	return 0;
}

int
verify_in_txn(MDB_txn *txn, const struct dataset *ds, int quiet)
{
	char h1[100];
	struct model m;
	MDB_dbi dbi;
	MDB_cursor *cur;
	MDB_stat st;
	MDB_val key, data;
	unsigned int flags;
	size_t k;
	int rc;

	rc = mdb_dbi_open(txn, ds->db, 0, &dbi);
	if (rc == MDB_NOTFOUND)
		return mismatch(ds->db, "database not found");
	check_rc(rc, "mdb_dbi_open");
	CHECK(mdb_dbi_flags(txn, dbi, &flags));
	if (flags != ds->dbflags)
		return mismatch(ds->db, "DB flags are 0x%x, expected 0x%x", flags, ds->dbflags);

	model_build(ds, &m);
	CHECK(mdb_stat(txn, dbi, &st));
	if (st.ms_entries != m.nentries) {
		rc = mismatch(ds->db, "mdb_stat entries %zu, expected %zu", st.ms_entries, m.nentries);
		goto done;
	}

	/* Point lookups: present keys return their first (smallest) value. */
	for (k = 0; k < m.nitems; k++) {
		key = m.items[k].key;
		rc = mdb_get(txn, dbi, &key, &data);
		if (rc == MDB_NOTFOUND) {
			rc = mismatch(ds->db, "mdb_get: key i=%u not found", m.items[k].i);
			goto done;
		}
		check_rc(rc, "mdb_get");
		if (!val_eq(&data, &m.items[k].vals[0])) {
			rc = mismatch(ds->db, "mdb_get: key i=%u has value %s", m.items[k].i,
				short_hex(&data, h1));
			goto done;
		}
	}
	/* Deleted keys are gone. */
	for (k = 0; k < m.nabsent; k++) {
		unsigned char kbuf[DS_MAX_KEY];
		key.mv_data = kbuf;
		key.mv_size = ds_key(ds, m.absent[k], kbuf);
		rc = mdb_get(txn, dbi, &key, &data);
		if (rc != MDB_NOTFOUND) {
			rc = mismatch(ds->db, "mdb_get: deleted key i=%u is present (rc=%d)",
				m.absent[k], rc);
			goto done;
		}
	}

	CHECK(mdb_cursor_open(txn, dbi, &cur));
	rc = check_walk(cur, ds, &m, MDB_FIRST, MDB_NEXT);
	if (!rc)
		rc = check_walk(cur, ds, &m, MDB_LAST, MDB_PREV);
	if (!rc && (ds->dbflags & MDB_DUPSORT))
		rc = check_dups(cur, ds, &m);
	/* MDB_SET_RANGE on an existing key lands on that key. */
	for (k = 0; !rc && k < m.nitems; k += 7) {
		key = m.items[k].key;
		CHECK(mdb_cursor_get(cur, &key, &data, MDB_SET_RANGE));
		if (!val_eq(&key, &m.items[k].key))
			rc = mismatch(ds->db, "MDB_SET_RANGE(i=%u) landed on %s", m.items[k].i,
				short_hex(&key, h1));
	}
	mdb_cursor_close(cur);
	if (!rc && !quiet)
		printf("verified db=%s keys=%zu entries=%zu absent=%zu\n",
			db_label(ds->db), m.nitems, m.nentries, m.nabsent);
done:
	model_free(&m);
	return rc;
}

int
cmd_verify(struct opts *o)
{
	MDB_env *env;
	MDB_txn *txn;
	int rc;

	parse_dataset_args(o, 1);
	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	rc = verify_in_txn(txn, &o->ds, 0);
	mdb_txn_abort(txn);
	mdb_env_close(env);
	return rc;
}

/* ---- delete ------------------------------------------------------------- */

int
cmd_delete(struct opts *o)
{
	struct dataset *ds = &o->ds;
	MDB_env *env;
	MDB_txn *txn;
	MDB_cursor *cur;
	MDB_dbi dbi;
	unsigned char *vbuf;
	uint32_t i, d, every = o->every, n = 0;
	size_t keys = 0, entries = 0;
	unsigned int txns = 1;
	int rc;

	parse_dataset_args(o, 1);
	if (!every)
		die(EXIT_USAGE, "--every must be > 0");
	env = open_env(o, 0);
	vbuf = malloc(max_value_size(ds));
	txn = begin_write(env);
	CHECK(mdb_dbi_open(txn, ds->db, 0, &dbi));
	CHECK(mdb_cursor_open(txn, dbi, &cur));

	for (i = 0; i < ds->count; i++) {
		unsigned char kbuf[DS_MAX_KEY];
		MDB_val key, data;
		uint32_t nd = ds_ndups(ds, i), left = 0;

		key.mv_data = kbuf;
		key.mv_size = ds_key(ds, i, kbuf);
		for (d = 0; d < nd; d++)
			left += ds_present(ds, i, d);
		if (!left)
			continue;	/* already deleted by a previous delete */

		if (ds_delete_key(every, i)) {
			/* Alternate between mdb_del() and a cursor delete of the
			 * whole key (all its values). */
			if ((i / every) % 2 == 0) {
				rc = mdb_del(txn, dbi, &key, NULL);
			} else {
				rc = mdb_cursor_get(cur, &key, &data, MDB_SET);
				if (!rc)
					rc = mdb_cursor_del(cur, (ds->dbflags & MDB_DUPSORT) ? MDB_NODUPDATA : 0);
			}
			if (rc == MDB_NOTFOUND)
				die(EXIT_MISMATCH, "delete: key i=%u not found", i);
			check_rc(rc, "delete key");
			keys++;
			entries += left;
		} else {
			for (d = 0; d < nd; d++) {
				if (!ds_present(ds, i, d) || !ds_delete_dup(ds, every, i, d))
					continue;
				data.mv_data = vbuf;
				data.mv_size = ds_vlen(ds, i, d);
				ds_value(ds, i, d, vbuf, data.mv_size);
				/* Alternate between mdb_del(key, data) and a cursor
				 * positioned with MDB_GET_BOTH. */
				if ((d / 2) % 2 == 0) {
					rc = mdb_del(txn, dbi, &key, &data);
				} else {
					MDB_val k2 = key;
					rc = mdb_cursor_get(cur, &k2, &data, MDB_GET_BOTH);
					if (!rc)
						rc = mdb_cursor_del(cur, 0);
				}
				if (rc == MDB_NOTFOUND)
					die(EXIT_MISMATCH, "delete: value %u of key i=%u not found", d, i);
				check_rc(rc, "delete value");
				entries++;
				if (--left == 0)
					keys++;
			}
		}
		if (o->per_txn && ++n % o->per_txn == 0 && i + 1 < ds->count) {
			mdb_cursor_close(cur);
			commit_write(txn);
			txn = begin_write(env);
			txns++;
			CHECK(mdb_cursor_open(txn, dbi, &cur));
		}
	}
	mdb_cursor_close(cur);
	printf("deleted db=%s keys=%zu entries=%zu txns=%u\n", db_label(ds->db), keys, entries, txns);
	commit_write(txn);
	free(vbuf);
	mdb_env_close(env);
	return 0;
}

/* ---- drop --------------------------------------------------------------- */

/* mdb_drop(): empty the DB, or with --delete delete it from the environment
 * (the main DB can only be emptied). Prints what it held. */
int
cmd_drop(struct opts *o)
{
	const char *db = strcmp(o->pos[1], "-") ? o->pos[1] : NULL;
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_stat st;
	int rc;

	env = open_env(o, 0);
	txn = begin_write(env);
	rc = mdb_dbi_open(txn, db, 0, &dbi);
	if (rc == MDB_NOTFOUND)
		die(EXIT_MISMATCH, "drop: database %s not found", db_label(db));
	check_rc(rc, "mdb_dbi_open");
	CHECK(mdb_stat(txn, dbi, &st));
	CHECK(mdb_drop(txn, dbi, o->del));
	printf("dropped db=%s delete=%d entries=%zu pages=%zu\n", db_label(db), o->del,
		st.ms_entries, st.ms_branch_pages + st.ms_leaf_pages + st.ms_overflow_pages);
	commit_write(txn);
	mdb_env_close(env);
	return 0;
}

/* ---- dump --------------------------------------------------------------- */

/* Is `key` of the main DB the name of a named DB? (as mdb_stat -a decides) */
static int
is_subdb(MDB_txn *txn, const MDB_val *key, char **name)
{
	MDB_dbi dbi;
	char *s;

	if (memchr(key->mv_data, '\0', key->mv_size))
		return 0;
	s = malloc(key->mv_size + 1);
	memcpy(s, key->mv_data, key->mv_size);
	s[key->mv_size] = '\0';
	if (mdb_dbi_open(txn, s, 0, &dbi)) {
		free(s);
		return 0;
	}
	*name = s;
	return 1;
}

/* Print "db=<label>" then one "<hexkey>=<hexvalue>" line per item, in cursor
 * order. Named-DB records of the main DB print as "<hexkey>=@db" because
 * their value (an MDB_db with page numbers) differs between copies. With
 * `names`, collect those DB names. */
static void
dump_db(MDB_txn *txn, const char *db, char ***names, size_t *nnames)
{
	MDB_dbi dbi;
	MDB_cursor *cur;
	MDB_val key, data;
	int rc;

	rc = mdb_dbi_open(txn, db, 0, &dbi);
	if (rc == MDB_NOTFOUND)
		die(EXIT_MISMATCH, "dump: database %s not found", db_label(db));
	check_rc(rc, "mdb_dbi_open");
	printf("db=%s\n", db_label(db));
	CHECK(mdb_cursor_open(txn, dbi, &cur));
	while ((rc = mdb_cursor_get(cur, &key, &data, MDB_NEXT)) == 0) {
		char *name;
		print_hex(stdout, key.mv_data, key.mv_size);
		if (!db && is_subdb(txn, &key, &name)) {
			fputs("=@db\n", stdout);
			if (names) {
				*names = realloc(*names, sizeof(char *) * (*nnames + 1));
				(*names)[(*nnames)++] = name;
			} else {
				free(name);
			}
			continue;
		}
		putchar('=');
		print_hex(stdout, data.mv_data, data.mv_size);
		putchar('\n');
	}
	if (rc != MDB_NOTFOUND)
		check_rc(rc, "mdb_cursor_get");
	mdb_cursor_close(cur);
}

int
cmd_dump(struct opts *o)
{
	MDB_env *env;
	MDB_txn *txn;

	if (o->all == (o->npos == 2))
		die(EXIT_USAGE, "usage: cpeer dump <env> <db|-> | cpeer dump <env> --all");
	env = open_env(o, MDB_RDONLY);
	CHECK(mdb_txn_begin(env, NULL, MDB_RDONLY, &txn));
	if (o->all) {
		char **names = NULL;
		size_t nnames = 0, k;
		dump_db(txn, NULL, &names, &nnames);
		for (k = 0; k < nnames; k++) {
			dump_db(txn, names[k], NULL, NULL);
			free(names[k]);
		}
		free(names);
	} else {
		dump_db(txn, strcmp(o->pos[1], "-") ? o->pos[1] : NULL, NULL, NULL);
	}
	mdb_txn_abort(txn);
	mdb_env_close(env);
	return 0;
}
