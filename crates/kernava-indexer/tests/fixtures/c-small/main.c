#include <stdio.h>
#include "helper.h"

int add(int a, int b) {
    return a + b;
}

static int helper(int x) {
    return x * 2;
}

int main() {
    int r = add(1, 2);
    printf("%d\n", r);
    return 0;
}
