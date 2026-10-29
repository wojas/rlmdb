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
 * described in rust/DESIGN.md and, in stage 2, with lmdb::format::layout.
 */
#include "mdb.c"

#include <stdalign.h>

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

int main(void)
{
	/* Lock-mode of this build: exactly one of these is defined by mdb.c. */
#if defined(MDB_USE_POSIX_SEM)
	printf("lock_mode=posix_sem\n");
#elif defined(MDB_USE_POSIX_MUTEX)
	printf("lock_mode=posix_mutex\n");
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
	PX("P_DIRTY", P_DIRTY);
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

	/* MDB_page: the common page header */
	SZ(MDB_page);
	OFF(MDB_page, mp_p.p_pgno);
	OFF(MDB_page, mp_pad);
	OFF(MDB_page, mp_flags);
	OFF(MDB_page, mp_pb.pb.pb_lower);
	OFF(MDB_page, mp_pb.pb.pb_upper);
	OFF(MDB_page, mp_pb.pb_pages);
	P("offsetof(MDB_page.mp_ptrs)", offsetof(MDB_page, mp_ptrs));

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
#if defined(_WIN32) || defined(MDB_USE_POSIX_SEM)
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
	OFF(MDB_txninfo, mt2);
#if defined(_WIN32) || defined(MDB_USE_POSIX_SEM)
	OFF(MDB_txninfo, mt1.mtb.mtb_rmname);
	OFF(MDB_txninfo, mt2.mt2_wmname);
#else
	OFF(MDB_txninfo, mt1.mtb.mtb_rmutex);
	OFF(MDB_txninfo, mt2.mt2_wmutex);
#endif
	OFF(MDB_txninfo, mti_readers);
	OFF(MDB_txninfo, mti_readers[1]);

	return 0;
}
