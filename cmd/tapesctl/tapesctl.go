// Package tapesctlcmder provides the root command for the tapesctl CLI.
package tapesctlcmder

import (
	"fmt"

	"github.com/spf13/cobra"
)

const message = "All in all, just another tape in the stereo"

// NewTapesctlCmd creates the root tapesctl command.
func NewTapesctlCmd() *cobra.Command {
	cmd := &cobra.Command{
		Use:   "tapesctl",
		Short: "Tapes control CLI",
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, _ []string) error {
			_, err := fmt.Fprintln(cmd.OutOrStdout(), message)
			return err
		},
	}

	return cmd
}
