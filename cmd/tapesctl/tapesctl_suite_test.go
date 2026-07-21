package tapesctlcmder_test

import (
	"testing"

	. "github.com/onsi/ginkgo/v2"
	. "github.com/onsi/gomega"
)

func TestTapesctl(t *testing.T) {
	RegisterFailHandler(Fail)
	RunSpecs(t, "Tapesctl Command Suite")
}
