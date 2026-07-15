struct Point {
    int x;
    int y;
};

int point_sum(struct Point *p) {
    return p->x + p->y;
}

enum Color { RED, GREEN, BLUE };

union Value {
    int i;
    float f;
};
