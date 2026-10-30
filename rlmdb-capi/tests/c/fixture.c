/* Writes the fixture environment used by capi_read.c and the tool
 * comparisons. Always linked against the C library (it writes).
 *
 *   fixture <dir>
 *
 * Named databases (and what they exercise):
 *   plain    100 keys "key000".."key099" -> "val000".., plus "big" holding
 *            3 pages + 100 bytes (overflow pages)
 *   dups     MDB_DUPSORT: "a" 1 item, "b" 3 items (sub-page), "c" 600 items
 *            (sub-database)
 *   fixed    MDB_DUPSORT|MDB_DUPFIXED, 4-byte big-endian items: "k1" 0..9999
 *            (several pages), "k2" one item, "k3" 1..5
 *   intkey   MDB_INTEGERKEY, size_t keys 1..50
 *   numeric  custom key order (decimal value): keys "1".."200"
 *   revkeys  custom key order (reverse of memcmp): keys "a".."z"
 *   rdup     MDB_DUPSORT with a reverse data order: "x" -> "a".."j"
 *   extra    one item, for the MDB_DBS_FULL test
 * The main DB also holds a plain key "notadb".
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "lmdb.h"
#include "cmpfuncs.h"

#define E(expr) do { int rc_ = (expr); if (rc_) { \
	fprintf(stderr, "%s:%d: %s: %s\n", __FILE__, __LINE__, #expr, mdb_strerror(rc_)); \
	exit(2); } } while (0)

static void put(MDB_txn *txn, MDB_dbi dbi, const void *k, size_t ks, const void *d, size_t ds)
{
	MDB_val key, data;
	key.mv_data = (void *)k; key.mv_size = ks;
	data.mv_data = (void *)d; data.mv_size = ds;
	E(mdb_put(txn, dbi, &key, &data, 0));
}

static void puts_(MDB_txn *txn, MDB_dbi dbi, const char *k, const char *d)
{
	put(txn, dbi, k, strlen(k), d, strlen(d));
}

int main(int argc, char **argv)
{
	MDB_env *env;
	MDB_txn *txn;
	MDB_dbi dbi;
	MDB_stat st;
	char k[32], d[32], *big;
	unsigned char be[4];
	size_t i, bigsize;

	if (argc != 2) {
		fprintf(stderr, "usage: fixture <dir>\n");
		return 2;
	}
	E(mdb_env_create(&env));
	E(mdb_env_set_maxdbs(env, 16));
	E(mdb_env_set_mapsize(env, 64UL << 20));
	E(mdb_env_open(env, argv[1], 0, 0664));
	E(mdb_env_stat(env, &st));
	E(mdb_txn_begin(env, NULL, 0, &txn));

	E(mdb_dbi_open(txn, "plain", MDB_CREATE, &dbi));
	for (i = 0; i < 100; i++) {
		sprintf(k, "key%03d", (int)i);
		sprintf(d, "val%03d", (int)i);
		puts_(txn, dbi, k, d);
	}
	bigsize = 3 * st.ms_psize + 100;
	big = malloc(bigsize);
	for (i = 0; i < bigsize; i++)
		big[i] = (char)(i * 7);
	put(txn, dbi, "big", 3, big, bigsize);
	free(big);

	E(mdb_dbi_open(txn, "dups", MDB_CREATE | MDB_DUPSORT, &dbi));
	puts_(txn, dbi, "a", "only");
	puts_(txn, dbi, "b", "b1");
	puts_(txn, dbi, "b", "b2");
	puts_(txn, dbi, "b", "b3");
	for (i = 0; i < 600; i++) {
		sprintf(d, "v%04d", (int)i);
		puts_(txn, dbi, "c", d);
	}

	E(mdb_dbi_open(txn, "fixed", MDB_CREATE | MDB_DUPSORT | MDB_DUPFIXED, &dbi));
	for (i = 0; i < 10000; i++) {
		be[0] = (unsigned char)(i >> 24); be[1] = (unsigned char)(i >> 16);
		be[2] = (unsigned char)(i >> 8); be[3] = (unsigned char)i;
		put(txn, dbi, "k1", 2, be, 4);
	}
	be[0] = be[1] = be[2] = 0; be[3] = 7;
	put(txn, dbi, "k2", 2, be, 4);
	for (i = 1; i <= 5; i++) {
		be[3] = (unsigned char)i;
		put(txn, dbi, "k3", 2, be, 4);
	}

	E(mdb_dbi_open(txn, "intkey", MDB_CREATE | MDB_INTEGERKEY, &dbi));
	for (i = 1; i <= 50; i++) {
		sprintf(d, "i%d", (int)i);
		put(txn, dbi, &i, sizeof(i), d, strlen(d));
	}

	E(mdb_dbi_open(txn, "numeric", MDB_CREATE, &dbi));
	E(mdb_set_compare(txn, dbi, numcmp));
	for (i = 1; i <= 200; i++) {
		sprintf(k, "%d", (int)i);
		puts_(txn, dbi, k, k);
	}

	E(mdb_dbi_open(txn, "revkeys", MDB_CREATE, &dbi));
	E(mdb_set_compare(txn, dbi, revcmp));
	for (i = 0; i < 26; i++) {
		k[0] = (char)('a' + i); k[1] = 0;
		puts_(txn, dbi, k, k);
	}

	E(mdb_dbi_open(txn, "rdup", MDB_CREATE | MDB_DUPSORT, &dbi));
	E(mdb_set_dupsort(txn, dbi, revcmp));
	for (i = 0; i < 10; i++) {
		d[0] = (char)('a' + i); d[1] = 0;
		puts_(txn, dbi, "x", d);
	}

	E(mdb_dbi_open(txn, "extra", MDB_CREATE, &dbi));
	puts_(txn, dbi, "e", "1");

	E(mdb_dbi_open(txn, NULL, 0, &dbi));
	puts_(txn, dbi, "notadb", "plain data");

	E(mdb_txn_commit(txn));
	mdb_env_close(env);
	return 0;
}
