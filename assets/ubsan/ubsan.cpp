#include <stdio.h>
#include <stdlib.h>

// Minimal null-UBSan handlers (no full sanitizer runtime on WASI).
extern "C" void __ubsan_handle_type_mismatch_v1(void *, void *ptr) {
  fprintf(stderr, "Null pointer dereference (address %p)\n", ptr);
  fflush(stderr);
  exit(1);
}

extern "C" void __ubsan_handle_type_mismatch_v1_abort(void *data, void *ptr) {
  __ubsan_handle_type_mismatch_v1(data, ptr);
  __builtin_unreachable();
}
