package main

import (
	"runtime"
	"testing"
)

func TestOS(t *testing.T) {
	if runtime.GOOS != "darwin" && runtime.GOOS != "linux" {
		t.Fatal(runtime.GOOS)
	}
}
