require 'json'
require_relative 'helper'

class Calculator
  def initialize(value)
    @value = value
  end

  def add(x, y)
    compute(x)
  end

  private

  def helper(x)
    x * 2
  end

  def compute(x)
    helper(x) + @value
  end
end

module Math
  def compute(x)
    x * 2
  end
end

def free_function(a, b)
  a + b
end
