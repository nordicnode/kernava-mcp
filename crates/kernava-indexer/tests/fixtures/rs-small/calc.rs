// calc.rs — struct with impl methods

pub struct Calculator {
    value: i32,
}

impl Calculator {
    pub fn new() -> Self {
        Calculator { value: 0 }
    }

    pub fn compute(&self, x: i32) -> i32 {
        if x > 0 {
            self.value + x
        } else {
            0
        }
    }
}
