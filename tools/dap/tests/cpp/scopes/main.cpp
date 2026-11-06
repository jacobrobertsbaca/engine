int getSample() {
  int list = 0;
  list = list; // before either loop — neither i nor expectedResult should be visible
  for (int i = 0; i < 3; i++) {
    list += i; // first loop — only this i should be visible
  }
  for (int i = 0; i < 3; i++) {
    int expectedResult = i;
    list += expectedResult;
  }
  return list;
}

int main() {
  return getSample();
}
