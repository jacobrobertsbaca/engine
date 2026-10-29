#include <cstring>
#include <deque>
#include <string>
#include <vector>

struct Large { int value; char padding[256]; };

int main() {
  std::deque<int> empty;
  std::deque<int> one{5};
  std::deque<int> drained{1};
  drained.pop_front();
  std::deque<int> numbers;
  for (int i = 0; i < 1030; ++i) numbers.push_back(i);
  numbers.pop_front();
  numbers.pop_front();
  numbers.push_front(7);
  std::deque<bool> bits(4100, false);
  bits[4095] = true;
  bits.pop_front();
  std::deque<std::string> words = {"first", "second", std::string(64, 'x')};
  words.pop_front();
  std::deque<std::vector<int>> nested = {{1, 2}, {3}};
  std::deque<Large> large(18);
  large[16].value = 42;
  int* invalid = reinterpret_cast<int*>(0xfffffff0);
  int marker = 37;
  std::deque<int*> pointers{&marker, nullptr, invalid};
  const auto& unreadable = *reinterpret_cast<std::deque<int>*>(0xfffffff0);
  std::deque<int> corrupt;
  unsigned char savedDeque[sizeof(corrupt)];
  std::memcpy(savedDeque, &corrupt, sizeof(corrupt));
  std::memset(static_cast<void*>(&corrupt), 0xff, sizeof(corrupt));
  std::memcpy(static_cast<void*>(&corrupt), savedDeque, sizeof(corrupt)); // breakpoint
  return 0;
}
