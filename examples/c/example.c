/* Example: decode an .mvdz through stdio callbacks (as an engine would
 * through its VFS), seek by time, read the metadata, and stream-compress
 * a demo back (with an extension metadata key). Usage: example file.mvd  (any QuakeWorld MVD demo). */
#define _FILE_OFFSET_BITS 64
#include "mvdz.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static long long f_read(void *u, void *buf, size_t n) {
    size_t k = fread(buf, 1, n, (FILE *)u);
    return ferror((FILE *)u) ? -1 : (long long)k;
}
static long long f_seek(void *u, long long off, int whence) {
    int w = whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : SEEK_END;
    /* 64-bit offsets (fseeko/ftello; _fseeki64 on Windows) */
    if (fseeko((FILE *)u, (off_t)off, w) != 0) return -1;
    return (long long)ftello((FILE *)u);
}
static long long f_write(void *u, const void *buf, size_t n) {
    return fwrite(buf, 1, n, (FILE *)u) == n ? (long long)n : -1;
}

static int fail(const char *what, int rc) {
    fprintf(stderr, "%s: %s (%d)\n", what, mvdz_strerror(rc), rc);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: %s demo.mvd\n", argv[0]); return 2; }
    FILE *f = fopen(argv[1], "rb");
    if (!f) { perror(argv[1]); return 1; }
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    unsigned char *demo = malloc(n);
    if (fread(demo, 1, n, f) != (size_t)n) return 1;
    fclose(f);
    printf("mvdz %s\n", mvdz_version());

    /* 1. stream-compress the demo in small chunks, as a recorder would */
    const char *packed = "example_out.mvdz";
    FILE *o = fopen(packed, "wb");
    int err = 0;
    mvdz_writer *w = mvdz_writer_open(f_write, o, MVDZ_LEVEL_NORMAL, 256 * 1024, 1, &err);
    if (!w) return fail("writer_open", err);
    /* extension metadata: where the demo came from (any time before finish) */
    int rc = mvdz_writer_set_ext(w, "source", "example.c", 9);
    if (rc) return fail("writer_set_ext", rc);
    for (long i = 0; i < n; i += 4000) {
        long k = n - i < 4000 ? n - i : 4000;
        int rc = mvdz_writer_write(w, demo + i, (size_t)k);
        if (rc) return fail("writer_write", rc);
    }
    rc = mvdz_writer_finish(w);
    if (rc) return fail("writer_finish", rc);
    long packed_n = ftell(o);
    fclose(o);
    printf("compressed %ld -> %ld bytes\n", n, packed_n);

    /* 2. metadata without decoding */
    FILE *p = fopen(packed, "rb");
    char *json; size_t json_n;
    rc = mvdz_info_json(f_read, f_seek, p, 0, &json, &json_n);
    if (rc) return fail("info_json", rc);
    if (!strstr(json, "\"source\":\"example.c\"")) { fprintf(stderr, "ext key missing\n"); return 1; }
    printf("metadata: %.120s...\n", json);
    mvdz_free(json, json_n);

    /* 3. decode through callbacks and compare */
    fseek(p, 0, SEEK_SET);
    mvdz_reader *r = mvdz_open_callbacks(f_read, f_seek, p, 2, 0, 0, &err);
    if (!r) return fail("open_callbacks", err);
    printf("raw size %lld, segments %lld\n", mvdz_raw_size(r), mvdz_segment_count(r));
    unsigned char *back = malloc(n + 1);
    long long got = 0, k;
    while ((k = mvdz_read(r, back + got, 7777 < n + 1 - got ? 7777 : (size_t)(n + 1 - got))) > 0) got += k;
    if (k < 0) return fail("read", (int)k);
    if (got != n || memcmp(back, demo, n) != 0) { fprintf(stderr, "MISMATCH\n"); return 1; }
    printf("decoded %lld bytes, identical\n", got);

    /* 4. seek to the middle of the demo by time */
    uint64_t t0, off;
    rc = mvdz_seek_time(r, 60000, &t0, &off);
    if (rc) return fail("seek_time", rc);
    got = 0;
    while ((k = mvdz_read(r, back + got, 65536)) > 0) got += k;
    if (k < 0 || (uint64_t)got != (uint64_t)n - off || memcmp(back, demo + off, got) != 0) {
        fprintf(stderr, "SEEK MISMATCH\n");
        return 1;
    }
    printf("seek to 60 s: segment starts at %llu ms, byte %llu; tail identical\n",
           (unsigned long long)t0, (unsigned long long)off);
    mvdz_close(r);
    mvdz_close(r); /* a closed handle is invalid, not a crash */
    if (mvdz_read(r, back, 10) != MVDZ_E_INVALID) { fprintf(stderr, "closed handle accepted\n"); return 1; }
    fclose(p);

    /* 5. attachments */
    const char *keys[] = {"x-recorded-by", "example.track"};
    const char *vals[] = {"someone", "1"};
    mvdz_attachment a = {"voice/someone.ogg", "audio/ogg; codecs=opus", keys, vals, 2, "OggS...", 7};
    rc = mvdz_attachments_update(packed, NULL, 0, &a, 1);
    if (rc) return fail("attachments_update", rc);
    p = fopen(packed, "rb");
    unsigned char *att; size_t att_n;
    rc = mvdz_attachment_read(f_read, f_seek, p, "voice/someone.ogg", 0, &att, &att_n);
    if (rc || att_n != 7 || memcmp(att, "OggS...", 7) != 0) return fail("attachment_read", rc);
    mvdz_free(att, att_n);
    fclose(p);
    printf("attachment round trip ok\n");

    /* 6. corrupt input never crashes */
    unsigned char junk[64] = "MVDZ\x01";
    r = mvdz_open_memory(junk, sizeof junk, 1, 0, 0, &err);
    printf("garbage: %s\n", r ? "opened?!" : mvdz_strerror(err));
    if (r) mvdz_close(r);
    /* 7. an aborted recording: no trailer, its segments still decode */
    o = fopen(packed, "wb");
    w = mvdz_writer_open(f_write, o, MVDZ_LEVEL_FAST, 64 * 1024, 1, &err);
    if (!w) return fail("writer_open", err);
    rc = mvdz_writer_write(w, demo, (size_t)(n / 2));
    if (rc) return fail("writer_write", rc);
    rc = mvdz_writer_abort(w);
    if (rc || mvdz_writer_finish(w) != MVDZ_E_INVALID) return fail("writer_abort", rc);
    fclose(o);
    printf("aborted recording ok\n");

    /* 8. whole-buffer API (.mvd or .mvd.gz input); input that is not a
     * demo is refused */
    unsigned char *z; size_t z_n;
    rc = mvdz_compress(demo, (size_t)n, MVDZ_LEVEL_FAST, 0, 1, &z, &z_n);
    if (rc) return fail("compress", rc);
    unsigned char *e; size_t e_n;
    rc = mvdz_decompress(z, z_n, 1, 0, 0, &e, &e_n);
    if (rc || e_n != (size_t)n || memcmp(e, demo, n) != 0) return fail("decompress", rc);
    mvdz_free(z, z_n);
    mvdz_free(e, e_n);
    rc = mvdz_compress("hello world", 11, MVDZ_LEVEL_NORMAL, 0, 1, &z, &z_n);
    if (rc != MVDZ_E_NOTMVD) return fail("compress of a non-demo", rc);
    printf("non-demo: %s\n", mvdz_strerror(rc));
    free(back);
    free(demo);
    remove(packed);
    printf("OK\n");
    return 0;
}
