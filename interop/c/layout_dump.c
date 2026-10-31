/* layout_dump.c - print the in-memory/on-disk layout of LMDB's structures.
 *
 * The structures that make up the data file and the lock file (MDB_page,
 * MDB_node, MDB_meta, MDB_db, MDB_txninfo, MDB_reader, ...) are private to
 * mdb.c, so this program includes mdb.c itself and then prints sizeof,
 * offsetof and alignof of every interesting field as `name=value` lines.
 *
 * It is built with exactly the flags of the library under test (see
 * build.rs), so the numbers are those of the C build on this platform.
 * tests/layout.rs parses the output and compares it with the format
 * described in DESIGN.md and, in stage 2, with lmdb::format::layout.
 *
 * The same source is built twice: as layout_dump against the LMDB 0.9
 * mdb.c, and as layout_dump10 against the LMDB 1.0 one (LMDB10 below).
 * The 1.0 build prints the same lines in the same order, so the two outputs
 * can be compared line by line, except where a field exists in one version
 * only (P_DIRTY; mt2 and the semaphore names with named semaphores), and
 * names the 1.0 field where it was renamed (mtb_mutexid for mtb_rmname).
 * Lines only 1.0 has follow its counterparts: the page header's mp_txnid,
 * MDB_ovpage, the parts of MDB_LOCK_FORMAT, and the maximum key size of new
 * environments at several page sizes (computed at open in 1.0, fixed in
 * 0.9). tests/oracle10.rs checks those.
 */
#include "mdb.c"

#include <stdalign.h>
#include <stdlib.h>
#include <unistd.h>

#define LMDB10	(MDB_VERSION_MAJOR >= 1)

#define P(name, value)	printf("%s=%llu\n", name, (unsigned long long)(value))
#define PX(name, value)	printf("%s=0x%llx\n", name, (unsigned long long)(value))

/* sizeof/alignof of a type */
#define SZ(type) do { \
	P("sizeof(" #type ")", sizeof(type)); \
	P("alignof(" #type ")", alignof(type)); \
} while (0)

/* offsetof/sizeof of a field (the field path may contain dots) */
#define OFF(type, field) do { \
	P("offsetof(" #type "." #field ")", offsetof(type, field)); \
	P("sizeof(" #type "." #field ")", sizeof(((type *)0)->field)); \
} while (0)

#if LMDB10
/* The maximum key size and node size of a new environment created with page
 * size psize (0: the OS page size), in a fresh per-process directory. */
static void
print_env_limits(const char *tmp, int psize)
{
	char dir[4096], file[4200], name[64];
	MDB_env *env;
	int rc;

	snprintf(dir, sizeof(dir), "%s/layout_dump10.XXXXXX", tmp);
	if (!mkdtemp(dir)) {
		perror("mkdtemp");
		exit(1);
	}
	if ((rc = mdb_env_create(&env)) ||
		(psize && (rc = mdb_env_set_pagesize(env, psize))) ||
		(rc = mdb_env_open(env, dir, 0, 0600))) {
		fprintf(stderr, "layout_dump10: env with page size %d: %s\n",
			psize, mdb_strerror(rc));
		exit(1);
	}
	if (psize)
		snprintf(name, sizeof(name), "%d", psize);
	else
		snprintf(name, sizeof(name), "os_page_size");
	printf("me_psize(%s)=%u\n", name, env->me_psize);
	printf("me_nodemax(%s)=%u\n", name, env->me_nodemax);
	printf("mdb_env_get_maxkeysize(%s)=%d\n", name, mdb_env_get_maxkeysize(env));
	mdb_env_close(env);
	snprintf(file, sizeof(file), "%s/data.mdb", dir);
	unlink(file);
	snprintf(file, sizeof(file), "%s/lock.mdb", dir);
	unlink(file);
	rmdir(dir);
}
#endif

int main(void)
{
	/* Lock-mode of this build: exactly one of these is defined by mdb.c. */
#if defined(MDB_USE_POSIX_SEM)
	printf("lock_mode=posix_sem\n");
#elif defined(MDB_USE_POSIX_MUTEX)
	printf("lock_mode=posix_mutex\n");
#elif defined(MDB_USE_SYSV_SEM)
	printf("lock_mode=sysv_sem\n");
#else
	printf("lock_mode=other\n");
#endif
#ifdef MDB_ROBUST_SUPPORTED
	P("MDB_ROBUST_SUPPORTED", 1);
#else
	P("MDB_ROBUST_SUPPORTED", 0);
#endif

	/* Constants */
	PX("MDB_MAGIC", MDB_MAGIC);
	P("MDB_DATA_VERSION", MDB_DATA_VERSION);
	P("MDB_LOCK_VERSION", MDB_LOCK_VERSION);
	PX("MDB_LOCK_FORMAT", MDB_LOCK_FORMAT);
#if LMDB10
	/* MDB_LOCK_FORMAT = version + MDB_lock_desc << MDB_LOCK_VERSION_BITS,
	 * where MDB_lock_desc encodes CACHELINE, sizeof(MDB_PID_T),
	 * sizeof(pthread_t), sizeof(MDB_txbody) and MDB_LOCK_TYPE. */
	P("MDB_LOCK_VERSION_BITS", MDB_LOCK_VERSION_BITS);
	P("MDB_lock_desc", MDB_lock_desc);
	P("MDB_LOCK_TYPE", MDB_LOCK_TYPE);
	P("MDB_RPAGE_CACHE", MDB_RPAGE_CACHE);
#endif
	P("MDB_PIDLOCK", MDB_PIDLOCK);
	P("MDB_DEVEL", MDB_DEVEL);
	P("PAGEHDRSZ", PAGEHDRSZ);
	P("PAGEBASE", PAGEBASE);
	P("NODESIZE", NODESIZE);
	P("MDB_MAXKEYSIZE", MDB_MAXKEYSIZE);
	P("MDB_MINKEYS", MDB_MINKEYS);
	P("CACHELINE", CACHELINE);
	P("MNAME_LEN", MNAME_LEN);
	P("DEFAULT_READERS", DEFAULT_READERS);
	P("DEFAULT_MAPSIZE", DEFAULT_MAPSIZE);
	P("MAX_PAGESIZE", MAX_PAGESIZE);
	P("NUM_METAS", NUM_METAS);
	P("CORE_DBS", CORE_DBS);
	PX("P_INVALID", P_INVALID);
	P("PGNO_TOPWORD", PGNO_TOPWORD);
	P("os_page_size", sysconf(_SC_PAGE_SIZE));

	/* Page flags */
	PX("P_BRANCH", P_BRANCH);
	PX("P_LEAF", P_LEAF);
	PX("P_OVERFLOW", P_OVERFLOW);
	PX("P_META", P_META);
#if LMDB10
	PX("P_DIRTY_OVF", P_DIRTY_OVF);
#else
	PX("P_DIRTY", P_DIRTY);
#endif
	PX("P_LEAF2", P_LEAF2);
	PX("P_SUBP", P_SUBP);
	PX("P_LOOSE", P_LOOSE);
	PX("P_KEEP", P_KEEP);

	/* Node flags */
	PX("F_BIGDATA", F_BIGDATA);
	PX("F_SUBDATA", F_SUBDATA);
	PX("F_DUPDATA", F_DUPDATA);

	/* Scalar types */
	SZ(pgno_t);
	SZ(txnid_t);
	SZ(indx_t);
	SZ(size_t);
	SZ(pid_t);
	SZ(pthread_t);
	SZ(pthread_mutex_t);
	SZ(unsigned int);
	SZ(mdb_mutex_t);
#if LMDB10
	SZ(mdb_size_t);
	SZ(mdb_hash_t);
#endif

	/* MDB_page: the common page header */
	SZ(MDB_page);
	OFF(MDB_page, mp_p.p_pgno);
#if LMDB10
	OFF(MDB_page, mp_txnid);
#endif
	OFF(MDB_page, mp_pad);
	OFF(MDB_page, mp_flags);
	OFF(MDB_page, mp_pb.pb.pb_lower);
	OFF(MDB_page, mp_pb.pb.pb_upper);
	OFF(MDB_page, mp_pb.pb_pages);
	P("offsetof(MDB_page.mp_ptrs)", offsetof(MDB_page, mp_ptrs));

#if LMDB10
	/* MDB_ovpage: the data of an F_BIGDATA node (0.9: a bare pgno_t) */
	SZ(MDB_ovpage);
	OFF(MDB_ovpage, op_pgno);
	OFF(MDB_ovpage, op_txnid);
	OFF(MDB_ovpage, op_pages);
#endif

	/* MDB_node: a key/data item in a branch or leaf page */
	SZ(MDB_node);
	OFF(MDB_node, mn_lo);
	OFF(MDB_node, mn_hi);
	OFF(MDB_node, mn_flags);
	OFF(MDB_node, mn_ksize);
	P("offsetof(MDB_node.mn_data)", offsetof(MDB_node, mn_data));

	/* MDB_db: a B-tree descriptor */
	SZ(MDB_db);
	OFF(MDB_db, md_pad);
	OFF(MDB_db, md_flags);
	OFF(MDB_db, md_depth);
	OFF(MDB_db, md_branch_pages);
	OFF(MDB_db, md_leaf_pages);
	OFF(MDB_db, md_overflow_pages);
	OFF(MDB_db, md_entries);
	OFF(MDB_db, md_root);

	/* MDB_meta: the content of meta pages 0 and 1, at offset PAGEHDRSZ */
	SZ(MDB_meta);
	OFF(MDB_meta, mm_magic);
	OFF(MDB_meta, mm_version);
	OFF(MDB_meta, mm_address);
	OFF(MDB_meta, mm_mapsize);
	OFF(MDB_meta, mm_dbs);
	OFF(MDB_meta, mm_dbs[0]);
	OFF(MDB_meta, mm_dbs[1]);
	OFF(MDB_meta, mm_last_pg);
	OFF(MDB_meta, mm_txnid);
	P("offsetof(MDB_metabuf.mb_metabuf.mm_meta)",
		offsetof(MDB_metabuf, mb_metabuf.mm_meta));

	/* Lock file: reader slots */
	SZ(MDB_rxbody);
	OFF(MDB_rxbody, mrb_txnid);
	OFF(MDB_rxbody, mrb_pid);
	OFF(MDB_rxbody, mrb_tid);
	SZ(MDB_reader);
	OFF(MDB_reader, mru.mrx.mrb_txnid);
	OFF(MDB_reader, mru.mrx.mrb_pid);
	OFF(MDB_reader, mru.mrx.mrb_tid);

	/* Lock file: header */
	SZ(MDB_txbody);
	OFF(MDB_txbody, mtb_magic);
	OFF(MDB_txbody, mtb_format);
	/* The reader lock: listed after mtb_format as in 0.9, although 1.0
	 * moved it after mtb_numreaders. */
#if LMDB10 && (defined(_WIN32) || defined(MDB_USE_POSIX_SEM))
	OFF(MDB_txbody, mtb_mutexid);
#elif LMDB10 && defined(MDB_USE_SYSV_SEM)
	OFF(MDB_txbody, mtb_semid);
	OFF(MDB_txbody, mtb_rlocked);
#elif defined(_WIN32) || defined(MDB_USE_POSIX_SEM)
	OFF(MDB_txbody, mtb_rmname);
#else
	OFF(MDB_txbody, mtb_rmutex);
#endif
	OFF(MDB_txbody, mtb_txnid);
	OFF(MDB_txbody, mtb_numreaders);
	SZ(MDB_txninfo);
	OFF(MDB_txninfo, mt1);
	OFF(MDB_txninfo, mt1.mtb.mtb_magic);
	OFF(MDB_txninfo, mt1.mtb.mtb_format);
	OFF(MDB_txninfo, mt1.mtb.mtb_txnid);
	OFF(MDB_txninfo, mt1.mtb.mtb_numreaders);
#if LMDB10 && (defined(_WIN32) || defined(MDB_USE_POSIX_SEM))
	/* 1.0 has no mt2 with named locks: the readers follow mt1. */
	OFF(MDB_txninfo, mt1.mtb.mtb_mutexid);
#elif LMDB10 && defined(MDB_USE_SYSV_SEM)
	OFF(MDB_txninfo, mt2);
	OFF(MDB_txninfo, mt1.mtb.mtb_semid);
	OFF(MDB_txninfo, mt2.mt2_wlocked);
#elif defined(_WIN32) || defined(MDB_USE_POSIX_SEM)
	OFF(MDB_txninfo, mt2);
	OFF(MDB_txninfo, mt1.mtb.mtb_rmname);
	OFF(MDB_txninfo, mt2.mt2_wmname);
#else
	OFF(MDB_txninfo, mt2);
	OFF(MDB_txninfo, mt1.mtb.mtb_rmutex);
	OFF(MDB_txninfo, mt2.mt2_wmutex);
#endif
	OFF(MDB_txninfo, mti_readers);
	OFF(MDB_txninfo, mti_readers[1]);

#if LMDB10
	{
		const char *tmp = getenv("TMPDIR");
		if (!tmp || !*tmp)
			tmp = "/tmp";
		print_env_limits(tmp, 0);
		print_env_limits(tmp, 4096);
		print_env_limits(tmp, 16384);
	}
#endif

	return 0;
}
