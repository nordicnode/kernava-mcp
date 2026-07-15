from .math import add, multiply
from .util import helper
from .calc import Calculator

def main():
    total = add(1, 2)
    product = multiply(3, 4)
    result = helper("hello world")
    calc = Calculator.create()
    total2 = calc.compute(3, 4)
    return len(result) + total + product + total2
