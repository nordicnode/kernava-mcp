using System;
using Example.Math;

namespace Example {
    public class Calculator {
        private int value;

        public Calculator(int v) {
            value = v;
        }

        public int Add(int x, int y) {
            return Compute(x);
        }

        private int Helper(int x) {
            return x * 2;
        }

        public int Compute(int x) {
            return Helper(x) + value;
        }
    }

    public interface IMath {
        int Compute(int x);
    }

    public enum Color {
        Red, Green, Blue
    }
}
