using System;

namespace Example {
    public class Main {
        public static void Run() {
            Calculator calc = new Calculator(5);
            int r = calc.Add(1, 2);
            Console.WriteLine(r);
        }
    }
}
