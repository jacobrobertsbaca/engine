#include <cstring>
#include <map>
#include <string>
#include <vector>

int main() {
  std::vector<int> empty;
  std::vector<bool> packed{true, false, true};
  int* invalid = reinterpret_cast<int*>(0xfffffff0);
  const auto& unreadable = *reinterpret_cast<std::vector<int>*>(0xfffffff0);
  std::vector<int> corrupt;
  unsigned char savedVector[sizeof(corrupt)];
  std::memcpy(savedVector, &corrupt, sizeof(corrupt));
  std::memset(static_cast<void*>(&corrupt), 0xff, sizeof(corrupt));
  std::map<int, int> corruptMap;
  unsigned char savedMap[sizeof(corruptMap)];
  std::memcpy(savedMap, &corruptMap, sizeof(corruptMap));
  std::memset(static_cast<void*>(&corruptMap), 0xff, sizeof(corruptMap));
  std::string oversized(65537, 'x');
  std::map<int, int> many;
  for (int i = 0; i < 10002; ++i) many[i] = i;
  std::memcpy(static_cast<void*>(&corrupt), savedVector, sizeof(corrupt)); // breakpoint
  std::memcpy(static_cast<void*>(&corruptMap), savedMap, sizeof(corruptMap));
  return 0;
}
