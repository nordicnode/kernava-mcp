package main

// MathResult is an exported interface.
type MathResult interface {
	Result() int
}

// MathOps is an exported struct implementing MathResult.
type MathOps struct {
	val int
}

// Result is an exported method with a value receiver.
func (m MathOps) Result() int {
	return m.val
}
