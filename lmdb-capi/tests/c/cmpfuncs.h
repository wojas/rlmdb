/* Custom comparison functions shared by fixture.c and capi_read.c. */
#ifndef CMPFUNCS_H
#define CMPFUNCS_H

#include <string.h>
#include "lmdb.h"

static long numval(const MDB_val *v)
{
	long n = 0;
	size_t i;
	for (i = 0; i < v->mv_size; i++)
		n = n * 10 + (((const char *)v->mv_data)[i] - '0');
	return n;
}

/* Keys are decimal numbers: "9" < "10". */
static int numcmp(const MDB_val *a, const MDB_val *b)
{
	long x = numval(a), y = numval(b);
	return x < y ? -1 : x > y;
}

/* The reverse of LMDB's default order. */
static int revcmp(const MDB_val *a, const MDB_val *b)
{
	size_t n = a->mv_size < b->mv_size ? a->mv_size : b->mv_size;
	int r = memcmp(a->mv_data, b->mv_data, n);
	if (r)
		return -r;
	return a->mv_size < b->mv_size ? 1 : a->mv_size > b->mv_size ? -1 : 0;
}

#endif
