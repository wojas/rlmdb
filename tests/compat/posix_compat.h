/* posix_compat.h - the POSIX calls of rlmdb's C test programs, on Windows.
 *
 * The test programs (interop/c, rlmdb-capi/tests/c, rlmdb-tools/tests/c,
 * bench/c) include this instead of <pthread.h>, <unistd.h>, <sys/stat.h>
 * and <dlfcn.h>. On POSIX systems it includes those headers and nothing
 * else. On Windows (MSVC and MinGW) it maps the few calls they use onto
 * the Windows API:
 *
 *   threads    pthread_create/join/self, mutexes and condition variables
 *              (SRW locks and CONDITION_VARIABLEs; static initialisers)
 *   files      mkdir(path, mode) (mode ignored), pread (ReadFile with an
 *              offset), O_BINARY for open() where C needs it, and
 *              COMPAT_CREATE_MODE, the permissions open() takes when it
 *              creates a file
 *   processes  getpid, sleep, usleep
 *   system     sysconf(_SC_PAGESIZE), setenv/unsetenv (on the process
 *              environment, which the Rust library reads)
 *   loading    dlopen/dlsym/dlclose/dlerror (LoadLibrary & co.;
 *              RTLD_NOLOAD is GetModuleHandleEx)
 *   stdio      stdin, stdout and stderr in binary mode from the start (a
 *              constructor), so that the programs read and write "\n" as
 *              on POSIX: their output is parsed by the tests. (The
 *              upstream LMDB tools do not include this; they keep the C
 *              runtime's text mode, as on any Windows system.)
 *              setvbuf(f, NULL, _IOLBF, 0) gets the default size.
 *
 * MSVC's C runtime (the UCRT) ends a program that passes a parameter it
 * considers invalid (its invalid-parameter handler), where POSIX and
 * MinGW's msvcrt accept it: a buffer of 0 bytes for setvbuf, permissions
 * other than _S_IREAD and _S_IWRITE for open(). Hence the last two.
 *
 * The test programs print size_t with %zu: their builds define
 * __USE_MINGW_ANSI_STDIO=1 with MinGW (MSVC's C runtime has it anyway).
 *
 * The pthread names are macros for compat_* functions, so a system
 * <pthread.h> (MinGW's winpthreads) included elsewhere does not clash.
 * Not a general emulation: only what the test programs need.
 */
#ifndef POSIX_COMPAT_H
#define POSIX_COMPAT_H

#ifndef _WIN32

#include <dlfcn.h>
#include <pthread.h>
#include <sys/stat.h>
#include <unistd.h>

#ifndef O_BINARY
#define O_BINARY	0
#endif

/* The permissions of a file a test program creates with open(). */
#define COMPAT_CREATE_MODE	0664

/* What C LMDB returns where a path's directory does not exist, and where a
 * file it creates exists already: errno values on POSIX, the system's
 * error codes on Windows. */
#define COMPAT_ENOENT_DIR	ENOENT
#define COMPAT_EEXIST		EEXIST

/* lmdb.h's mdb_filehandle_t of a C runtime descriptor, and the value of
 * none: the descriptor itself on POSIX, its HANDLE on Windows. */
#define COMPAT_FILEHANDLE(fd)	(fd)
#define COMPAT_NO_FILEHANDLE	(-1)

/* The size of the file behind an mdb_filehandle_t, -1 on error. */
static __inline long long
compat_filehandle_size(int fd)
{
	struct stat sb;
	return fstat(fd, &sb) ? -1 : (long long)sb.st_size;
}

#else /* _WIN32 */

#include <windows.h>
#include <direct.h>
#include <errno.h>
#include <fcntl.h>
#include <io.h>
#include <process.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

/* Helpers are inline so that unused ones draw no warnings. */
#define COMPAT_FN	static __inline

#ifdef _MSC_VER
/* (Also declared by tests/compat/msvc/unistd.h, under the same guard.) */
#ifndef _SSIZE_T_DEFINED
#define _SSIZE_T_DEFINED
typedef intptr_t ssize_t;
#endif
/* GCC attributes in the test programs' prototypes (format, noreturn). */
#define __attribute__(x)
#endif

/* ---- stdio ---------------------------------------------------------------- */

COMPAT_FN void
compat_binary_stdio(void)
{
	_setmode(0, _O_BINARY);
	_setmode(1, _O_BINARY);
	_setmode(2, _O_BINARY);
}

/* setvbuf(stream, NULL, _IOLBF, 0), the programs' line buffering of the
 * default size: the Windows C runtime has no line buffering (_IOLBF
 * buffers fully; the lines a reader waits for are flushed), and the UCRT
 * refuses a buffer size below 2. The default size, BUFSIZ, then. */
COMPAT_FN int
compat_setvbuf(FILE *stream, char *buf, int mode, size_t size)
{
	if (mode != _IONBF && size < 2)
		size = BUFSIZ;
	return setvbuf(stream, buf, mode, size);
}
#define setvbuf(stream, buf, mode, size)	compat_setvbuf(stream, buf, mode, size)

#ifdef _MSC_VER
/* A C runtime initialiser: one per program (selectany; several translation
 * units include this), kept by the linker (/include). */
#pragma section(".CRT$XCU", read)
__declspec(allocate(".CRT$XCU")) __declspec(selectany)
void (__cdecl *compat_binary_stdio_init)(void) = compat_binary_stdio;
#pragma comment(linker, "/include:compat_binary_stdio_init")
#else
__attribute__((constructor)) static void
compat_binary_stdio_init(void)
{
	compat_binary_stdio();
}
#endif

/* ---- threads ------------------------------------------------------------ */

typedef HANDLE compat_thread_t;
typedef SRWLOCK compat_mutex_t;
typedef CONDITION_VARIABLE compat_cond_t;

struct compat_thread_start {
	void *(*fn)(void *);
	void *arg;
};

COMPAT_FN unsigned __stdcall
compat_thread_main(void *p)
{
	struct compat_thread_start s = *(struct compat_thread_start *)p;
	free(p);
	s.fn(s.arg);
	return 0;
}

COMPAT_FN int
compat_thread_create(compat_thread_t *t, const void *attr,
	void *(*fn)(void *), void *arg)
{
	struct compat_thread_start *s = malloc(sizeof(*s));
	(void)attr;
	if (!s)
		return ENOMEM;
	s->fn = fn;
	s->arg = arg;
	*t = (HANDLE)_beginthreadex(NULL, 0, compat_thread_main, s, 0, NULL);
	if (!*t) {
		free(s);
		return EAGAIN;
	}
	return 0;
}

COMPAT_FN int
compat_thread_join(compat_thread_t t, void **ret)
{
	if (ret)
		*ret = NULL;
	if (WaitForSingleObject(t, INFINITE) != WAIT_OBJECT_0)
		return EINVAL;
	CloseHandle(t);
	return 0;
}

COMPAT_FN int compat_mutex_init(compat_mutex_t *m, const void *a)
	{ (void)a; InitializeSRWLock(m); return 0; }
COMPAT_FN int compat_mutex_destroy(compat_mutex_t *m) { (void)m; return 0; }
COMPAT_FN int compat_mutex_lock(compat_mutex_t *m) { AcquireSRWLockExclusive(m); return 0; }
COMPAT_FN int compat_mutex_unlock(compat_mutex_t *m) { ReleaseSRWLockExclusive(m); return 0; }
COMPAT_FN int compat_cond_init(compat_cond_t *c, const void *a)
	{ (void)a; InitializeConditionVariable(c); return 0; }
COMPAT_FN int compat_cond_destroy(compat_cond_t *c) { (void)c; return 0; }
COMPAT_FN int compat_cond_wait(compat_cond_t *c, compat_mutex_t *m)
	{ return SleepConditionVariableSRW(c, m, INFINITE, 0) ? 0 : EINVAL; }
COMPAT_FN int compat_cond_signal(compat_cond_t *c) { WakeConditionVariable(c); return 0; }
COMPAT_FN int compat_cond_broadcast(compat_cond_t *c) { WakeAllConditionVariable(c); return 0; }

#define pthread_t			compat_thread_t
#define pthread_mutex_t			compat_mutex_t
#define pthread_cond_t			compat_cond_t
#define PTHREAD_MUTEX_INITIALIZER	SRWLOCK_INIT
#define PTHREAD_COND_INITIALIZER	CONDITION_VARIABLE_INIT
#define pthread_create			compat_thread_create
#define pthread_join			compat_thread_join
/* The id C LMDB stores in a reader slot's mr_tid (GetCurrentThreadId). */
#define pthread_self()			((size_t)GetCurrentThreadId())
#define pthread_mutex_init		compat_mutex_init
#define pthread_mutex_destroy		compat_mutex_destroy
#define pthread_mutex_lock		compat_mutex_lock
#define pthread_mutex_unlock		compat_mutex_unlock
#define pthread_cond_init		compat_cond_init
#define pthread_cond_destroy		compat_cond_destroy
#define pthread_cond_wait		compat_cond_wait
#define pthread_cond_signal		compat_cond_signal
#define pthread_cond_broadcast		compat_cond_broadcast

/* ---- files -------------------------------------------------------------- */

/* POSIX mkdir; Windows has no permissions to give. (After <direct.h> and
 * <io.h>, which declare the one-argument form.) */
#define mkdir(path, mode)	_mkdir(path)

/* pread: ReadFile at an offset (the descriptor's position is moved, which
 * the test programs do not rely on). */
COMPAT_FN ssize_t
pread(int fd, void *buf, size_t len, long long off)
{
	OVERLAPPED ov;
	DWORD got = 0;
	HANDLE h = (HANDLE)_get_osfhandle(fd);
	if (h == INVALID_HANDLE_VALUE) {
		errno = EBADF;
		return -1;
	}
	memset(&ov, 0, sizeof(ov));
	ov.Offset = (DWORD)off;
	ov.OffsetHigh = (DWORD)(off >> 32);
	if (len > 0x40000000)
		len = 0x40000000;
	if (!ReadFile(h, buf, (DWORD)len, &got, &ov)) {
		if (GetLastError() == ERROR_HANDLE_EOF)
			return 0;
		errno = EIO;
		return -1;
	}
	return (ssize_t)got;
}

/* open() of the Windows C runtime takes only these permissions (the UCRT
 * ends a program that passes others, such as 0664). */
#define COMPAT_CREATE_MODE	(_S_IREAD | _S_IWRITE)

/* ERROR_PATH_NOT_FOUND and ERROR_FILE_EXISTS (see the POSIX branch). */
#define COMPAT_ENOENT_DIR	3
#define COMPAT_EEXIST		80

/* lmdb.h's mdb_filehandle_t (a HANDLE) of a C runtime descriptor, and
 * INVALID_HANDLE_VALUE. */
#define COMPAT_FILEHANDLE(fd)	((void *)_get_osfhandle(fd))
#define COMPAT_NO_FILEHANDLE	((void *)(intptr_t)-1)

/* The size of the file behind a HANDLE, -1 on error. */
COMPAT_FN long long
compat_filehandle_size(void *h)
{
	LARGE_INTEGER size;
	return GetFileSizeEx((HANDLE)h, &size) ? size.QuadPart : -1;
}

/* ---- processes ------------------------------------------------------------ */

#define getpid()	_getpid()
#define sleep(s)	Sleep((DWORD)(s) * 1000)
#define usleep(us)	Sleep((DWORD)((us) / 1000))

/* ---- system --------------------------------------------------------------- */

#define _SC_PAGESIZE	30

COMPAT_FN long
sysconf(int name)
{
	SYSTEM_INFO si;
	if (name != _SC_PAGESIZE)
		return -1;
	GetSystemInfo(&si);
	return (long)si.dwPageSize;
}

/* setenv/unsetenv on the process environment, which is what the Rust
 * library reads (std::env); an empty value stays a set, empty variable as
 * with POSIX setenv. The C runtime's copy (getenv) is not used by the
 * test programs. */
COMPAT_FN int
setenv(const char *name, const char *value, int overwrite)
{
	if (!overwrite && GetEnvironmentVariableA(name, NULL, 0) > 0)
		return 0;
	return SetEnvironmentVariableA(name, value) ? 0 : -1;
}

COMPAT_FN int
unsetenv(const char *name)
{
	return SetEnvironmentVariableA(name, NULL) || GetLastError() == ERROR_ENVVAR_NOT_FOUND ? 0 : -1;
}

/* ---- loading -------------------------------------------------------------- */

/* Windows has one way to load a DLL; the modes only matter for NOLOAD. */
#define RTLD_LAZY	1
#define RTLD_NOW	2
#define RTLD_NOLOAD	4
#define RTLD_GLOBAL	0x100
#define RTLD_LOCAL	0

/* The text dlerror() returns (one buffer per translation unit). */
COMPAT_FN char *
compat_dlerror_buf(void)
{
	static char buf[64];
	return buf;
}

COMPAT_FN void *
dlopen(const char *file, int mode)
{
	HMODULE h = NULL;
	if (mode & RTLD_NOLOAD) {
		/* Already loaded? Takes a reference, as dlopen does. */
		if (!GetModuleHandleExA(0, file, &h))
			h = NULL;
	} else {
		h = LoadLibraryA(file);
	}
	if (!h)
		snprintf(compat_dlerror_buf(), 64,
			"Windows error %lu", (unsigned long)GetLastError());
	return (void *)h;
}

COMPAT_FN void *
dlsym(void *lib, const char *name)
{
	FARPROC p = GetProcAddress((HMODULE)lib, name);
	if (!p)
		snprintf(compat_dlerror_buf(), 64,
			"Windows error %lu", (unsigned long)GetLastError());
	return (void *)(intptr_t)p;
}

COMPAT_FN int
dlclose(void *lib)
{
	return FreeLibrary((HMODULE)lib) ? 0 : -1;
}

COMPAT_FN char *
dlerror(void)
{
	return compat_dlerror_buf();
}

#endif /* _WIN32 */

#endif /* POSIX_COMPAT_H */
