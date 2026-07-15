#include <iostream>

class Calculator {
public:
    Calculator(int v) : value(v) {}
    int add(int x, int y) { return x + y; }
private:
    int value;
};

namespace math {
    int compute(int x) { return x * 2; }
}

struct Point {
    int x;
    int y;
};

int main() {
    Calculator c(1);
    int r = c.add(1, 2);
    Point p;
    p.x = 1;
    return 0;
}
