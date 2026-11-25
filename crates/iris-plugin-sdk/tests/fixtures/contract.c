/* Freestanding C ABI fixture; no WASI, libc, filesystem or network imports. */
#include <stdint.h>
#include <stddef.h>
#define EXPORT(name) __attribute__((export_name(name)))
_Alignas(8) static unsigned char heap[262144];
static uint32_t used;

EXPORT("iris_abi_version") uint32_t version(void) { return 2; }
EXPORT("iris_alloc") uint32_t allocate(uint32_t len) {
    if (!len || len > 65536) return 0;
    uint32_t size = (len + 8 + 7) & ~7u;
    if (size > sizeof(heap) - used) return 0;
    uint32_t previous = used;
    used += size;
    uint32_t *header = (uint32_t *)(heap + previous);
    header[0] = previous; header[1] = len;
    return (uint32_t)(uintptr_t)(header + 2);
}
EXPORT("iris_dealloc") void release(uint32_t ptr, uint32_t len) {
    if (!ptr && !len) return;
    uint32_t *header = ((uint32_t *)(uintptr_t)ptr) - 2;
    if (header[1] != len || header[0] + ((len + 15) & ~7u) != used) __builtin_trap();
    used = header[0];
}
static size_t length(const char *text) { size_t n = 0; while(text[n]) n++; return n; }
static const char *find(const char *data, uint32_t len, const char *needle) {
    size_t n = length(needle);
    for (uint32_t p = 0; p + n <= len; p++) {
        size_t k = 0; while(k < n && data[p+k] == needle[k]) k++;
        if (k == n) return data+p+n;
    }
    return 0;
}
static uint64_t output(const char *data, uint32_t len) {
    uint32_t ptr = allocate(len);
    if (!ptr) __builtin_trap();
    for(uint32_t i=0; i<len; i++) ((char *)(uintptr_t)ptr)[i] = data[i];
    return ((uint64_t)ptr << 32) | len;
}
EXPORT("iris_on_event") uint64_t event(uint32_t ptr, uint32_t len) {
    const char *input = (const char *)(uintptr_t)ptr;
    const char *digits = find(input, len, "\"id\":\"");
    if (!find(input, len, "\"decimal\"")) return output("{\"ok\":[]}", 9);
    uint64_t value=0; uint32_t count=0; int valid = digits != 0;
    if (digits) {
        while(digits+count < input+len && digits[count] >= '0' && digits[count] <= '9') {
            uint32_t digit=(uint32_t)(digits[count]-'0');
            if (value > (UINT64_MAX-digit)/10) valid=0;
            value=value*10+digit; count++;
        }
        if (!count || (count>1 && digits[0]=='0') || digits+count>=input+len || digits[count]!='"') valid=0;
    }
    if (!valid) {
        const char *error="{\"error\":{\"code\":-1,\"message\":\"invalid input\"}}";
        return output(error, (uint32_t)length(error));
    }
    char text[128];
    const char *prefix="{\"ok\":[{\"command\":\"show_toast\",\"message\":\"";
    uint32_t n=(uint32_t)length(prefix);
    for(uint32_t i=0;i<n;i++) text[i]=prefix[i];
    for(uint32_t i=0;i<count;i++) text[n++]=digits[i];
    const char *suffix="\"}]}";
    for(uint32_t i=0;i<4;i++) text[n++]=suffix[i];
    return output(text,n);
}
