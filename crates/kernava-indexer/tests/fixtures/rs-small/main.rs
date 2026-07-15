// main.rs — entry point with cross-file calls

mod math;
mod util;
mod calc;

use math::{add, multiply};
use util::helper;
use calc::Calculator;

fn main() {
    let total = add(1, 2);
    let product = multiply(3, 4);
    let result = helper("hello");
    let calc = Calculator::new();
    let computed = calc.compute(42);
    println!("{}", total + product);
}
