package tapesctlcmder_test

import (
	"bytes"

	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"

	tapesctlcmder "github.com/papercomputeco/tapesctl/cmd/tapesctl"
)

var _ = Describe("Tapesctl command", func() {
	It("prints the tapesctl message", func() {
		var output bytes.Buffer

		cmd := tapesctlcmder.NewTapesctlCmd()
		cmd.SetArgs([]string{})
		cmd.SetOut(&output)

		err := cmd.Execute()
		Expect(err).NotTo(HaveOccurred())
		Expect(output.String()).To(Equal("All in all, just another tape in the stereo\n"))
	})
})
