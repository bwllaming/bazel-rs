package main

import (
	"fmt"
	"runtime"
)

func main() {
	fmt.Println("go", runtime.GOOS, runtime.GOARCH)
}
