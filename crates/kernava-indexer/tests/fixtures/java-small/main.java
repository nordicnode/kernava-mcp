package com.example;

public class Main {
    public static void main(String[] args) {
        Calculator calc = new Calculator(5);
        int r = calc.add(1, 2);
        System.out.println(r);
    }
}
