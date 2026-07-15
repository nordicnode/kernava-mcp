package com.example;

import java.util.List;

public class Calculator {
    private int value;

    public Calculator(int v) {
        this.value = v;
    }

    public int add(int x, int y) {
        return compute(x);
    }

    private int helper(int x) {
        return x * 2;
    }

    public int compute(int x) {
        return helper(x) + value;
    }
}

interface Math {
    int compute(int x);
}

enum Color {
    RED, GREEN, BLUE
}
