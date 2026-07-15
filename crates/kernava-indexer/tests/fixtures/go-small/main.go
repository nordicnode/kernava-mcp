package main

import "fmt"

// Add is an exported free function.
func Add(a int, b int) int {
	return a + b
}

// helper is an unexported free function.
func helper(x int) int {
	return x * 2
}

// main is the entry point — calls Add and helper in the same file.
func main() {
	r := Add(1, 2)
	h := helper(r)
	fmt.Println(h)
}
