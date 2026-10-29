import { createHash } from 'node:crypto';

/** Six translation units exercise libc++ without a PCH. Edits have equal byte length. */
export function sixTranslationUnits(variant = 'base') {
  const headerValue = variant === 'header' ? 20 : 10;
  const firstValue = variant === 'edit' || variant === 'header' ? 9 : 1;
  const files = {
    'common.h': `#pragma once\n#define HEADER_VALUE ${headerValue}\nint part1(); int part2(); int part3(); int part4(); int part5();\n`,
    'main.cpp': `#include <cstdio>
#include <fcntl.h>
#include <unistd.h>
#include "common.h"
static int invocations = 0;
int main() {
  const int value = HEADER_VALUE + part1() + part2() + part3() + part4() + part5();
  FILE* previous = fopen("/run-state.txt", "r");
  const int oldFile = previous != nullptr;
  if (previous) fclose(previous);
  FILE* current = fopen("/run-state.txt", "w");
  if (!current) return 3;
  fputs("fresh", current); fclose(current);
  printf("CACHE RESULT: %d GLOBAL: %d OLD_FILE: %d\\n", value, ++invocations, oldFile);
  const int device = open("/dev/debugger-sh-host", O_RDWR);
  const char draw[] = "RECT 20 20 150 80 #2563eb\\n";
  if (device < 0 || write(device, draw, sizeof(draw) - 1) != sizeof(draw) - 1) return 4;
  close(device);
  return invocations == 1 && oldFile == 0 ? 0 : 2;
}
`
  };
  for (let index = 1; index <= 5; index++) {
    files[`part${index}.cpp`] = `#include <vector>\n#include <string>\n#include "common.h"\nint part${index}() { std::vector<int> values{${index === 1 ? firstValue : index}}; return values.front(); }\n`;
  }
  return files;
}

export const expectedMarker = (variant = 'base') =>
  `CACHE RESULT: ${variant === 'header' ? 43 : variant === 'edit' ? 33 : 25} GLOBAL: 1 OLD_FILE: 0`;

export function snapshotHash(files) {
  // Explicit path/content pairs are deterministic and include additions/deletions/renames.
  return createHash('sha256').update(JSON.stringify(Object.entries(files).sort(([a], [b]) => a.localeCompare(b)))).digest('hex');
}

export const diagnosticFixtures = {
  message: { 'main.cpp': '#pragma message("CACHE DIAGNOSTIC RECEIPT")\nint main() { return 0; }\n' },
  warning: { 'main.cpp': '#warning CACHE_WARNING_RECEIPT\nint main() { return 0; }\n' },
  error: { 'main.cpp': '#error CACHE_ERROR_RECEIPT\nint main() { return 0; }\n' },
  nonce: (nonce) => ({
    'main.cpp': '#include "webide_test_config.h"\nint main() { return WEBIDE_TEST_NONCE == 0; }\n',
    'webide_test_config.h': `#pragma clang diagnostic push\n#pragma clang diagnostic ignored "-Wunused-variable"\n#define WEBIDE_TEST_NONCE ${nonce}\n#pragma clang diagnostic pop\n`
  })
};
