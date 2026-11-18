#include <stdio.h>
#include "shared/bar.h"
#include "shared/foo.h"
int main(void) {
  printf("%d\n", foo() + bar());
  return 0;
}
