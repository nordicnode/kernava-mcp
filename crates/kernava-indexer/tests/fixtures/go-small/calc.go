package main

// Calculator is a struct with value and pointer methods.
type Calculator struct {
	value int
}

// Add is an exported method with a value receiver.
func (c Calculator) Add(x int) int {
	return c.value + x
}

// subtract is an unexported method with a pointer receiver.
func (c *Calculator) subtract(x int) int {
	return c.value - x
}

// compute calls Add and subtract in the same file.
func (c Calculator) compute(x int) int {
	r := c.Add(x)
	r2 := c.subtract(x)
	return r + r2
}
