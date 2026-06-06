package main

import "fmt"

func main() {
	// Deliberate error: a string literal cannot initialize an `int`, so gopls
	// must report a type-mismatch diagnostic on this line.
	var x int = "not an int"
	fmt.Println(x)
}
