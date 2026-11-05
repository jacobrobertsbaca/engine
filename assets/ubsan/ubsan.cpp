#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>

// Match LLVM __ubsan::SourceLocation / TypeMismatchData on wasm32 (4-byte pointers).
struct SourceLocation {
  const char *filename;
  uint32_t line;
  uint32_t column;
};

struct TypeMismatchData {
  SourceLocation loc;
  const void *type;
  uint8_t log_alignment;
  uint8_t type_check_kind;
};

static void report(TypeMismatchData *data, void *ptr) {
  const char *file = data && data->loc.filename ? data->loc.filename : "<unknown>";
  uint32_t line = data ? data->loc.line : 0;
  fprintf(stderr, "Null pointer dereference (address %p) at %s:%u\n", ptr, file, line);
  fflush(stderr);
  exit(1);
}

extern "C" void __ubsan_handle_type_mismatch_v1(TypeMismatchData *data, void *ptr) {
  report(data, ptr);
}

extern "C" void __ubsan_handle_type_mismatch_v1_abort(TypeMismatchData *data, void *ptr) {
  report(data, ptr);
  __builtin_unreachable();
}
