package main

import (
	"fmt"
	"os"
)

// This file COMPILES. Everything wrong with it is found by an analyzer, not the
// compiler — which is the point: two servers look at the same text and each sees
// something the other does not.
func Greet(name string) {
	// gopls (printf analyzer): %d against a string.
	fmt.Printf("hello %d\n", name)
}

func Configure() {
	// golangci-lint (errcheck): the returned error is dropped on the floor.
	os.Mkdir("/tmp/bemtvi-example", 0o755)

	// golangci-lint (ineffassign): assigned, then overwritten before any read.
	count := 1
	count = 2
	fmt.Println( count ) // gofmt: the padding inside the parens comes out
}

func main() {
	Greet("world")
	Configure()
}
