/* Adapt GNU .eh_frame table registration to LLVM libunwind's single-FDE API.
 * LuaJIT uses the GNU convention on Linux, including static musl builds.
 * See llvm-project/libunwind/src/UnwindLevel1-gcc-ext.c (__register_frame).
 * Inputs have the original API's precondition: a valid, live FDE or a valid
 * zero-terminated .eh_frame table. Registration and removal visit identical
 * entries without copying or moving the PC-relative data.
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* The target C compiler redirects callers, but this adapter calls LLVM. */
#undef __register_frame
#undef __deregister_frame
extern void __register_frame(const void *);
extern void __deregister_frame(const void *);

typedef void (*frame_visitor)(const void *, void *);

static size_t entry_size(const unsigned char *entry, int *is_cie)
{
    uint32_t length;
    memcpy(&length, entry, sizeof length);
    if (!length) return 0;
    if (length == UINT32_MAX) {
        uint64_t extended_length, id;
        memcpy(&extended_length, entry + 4, sizeof extended_length);
        if (extended_length < sizeof id || extended_length > SIZE_MAX - 12)
            abort();
        memcpy(&id, entry + 12, sizeof id);
        *is_cie = id == 0;
        return (size_t)extended_length + 12;
    }
    if (length < sizeof(uint32_t)) abort();
    uint32_t id;
    memcpy(&id, entry + 4, sizeof id);
    *is_cie = id == 0;
    return (size_t)length + 4;
}

void lector_visit_eh_frames(const void *frames, frame_visitor visit, void *context)
{
    const unsigned char *entry = frames;
    int is_cie = 0;
    size_t size = entry_size(entry, &is_cie);
    if (!size) return;
    if (!is_cie) {
        visit(entry, context);
        return;
    }
    do {
        if (!is_cie) visit(entry, context);
        entry += size;
        size = entry_size(entry, &is_cie);
    } while (size);
}

static void register_frame(const void *frame, void *context)
{
    (void)context;
    __register_frame(frame);
}

static void deregister_frame(const void *frame, void *context)
{
    (void)context;
    __deregister_frame(frame);
}

void lector_register_eh_frames(const void *frames)
{
    lector_visit_eh_frames(frames, register_frame, NULL);
}

void lector_deregister_eh_frames(const void *frames)
{
    lector_visit_eh_frames(frames, deregister_frame, NULL);
}
