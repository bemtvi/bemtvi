package main

import "fmt"

// Select the loop below (`V3j` from the `sum := 0` line) and press <leader>ca —
// gopls offers "Extract function" / "Extract variable" only because the request
// carried the selection as its range.
func Report(values []int) {
	sum := 0
	for _, v := range values {
		sum += v
	}
	fmt.Println("total:", sum)
}

func main() {
	Report([]int{1, 2, 3})
}
