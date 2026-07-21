package main

import (
	"fmt"
	"os"

	tapesctlcmder "github.com/papercomputeco/tapesctl/cmd/tapesctl"
)

func main() {
	cmd := tapesctlcmder.NewTapesctlCmd()
	if err := cmd.Execute(); err != nil {
		fmt.Printf("Error executing root command: %v\n", err)
		os.Exit(1)
	}
}
