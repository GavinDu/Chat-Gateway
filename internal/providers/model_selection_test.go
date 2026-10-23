package providers

import (
	"testing"

	"github.com/go-rod/rod"
)

func TestSelectRequestedModelAttemptsNonDefaultProviderLabel(t *testing.T) {
	var attempted string
	ok := selectRequestedModel(nil, "metaai", "Llama Future", []string{"meta-ai", "llama"}, func(_ *rod.Page, model string) bool {
		attempted = model
		return true
	})
	if !ok {
		t.Fatal("selectRequestedModel() = false")
	}
	if attempted != "Llama Future" {
		t.Fatalf("selection attempt model = %q", attempted)
	}
}

func TestSelectRequestedModelSkipsFixedDefault(t *testing.T) {
	called := false
	ok := selectRequestedModel(nil, "metaai", "meta-ai", []string{"meta-ai", "llama"}, func(_ *rod.Page, _ string) bool {
		called = true
		return true
	})
	if !ok {
		t.Fatal("default model should be accepted")
	}
	if called {
		t.Fatal("default model unexpectedly triggered a picker selection")
	}
}

func TestSelectRequestedModelFailsWhenPickerCannotHonorLabel(t *testing.T) {
	ok := selectRequestedModel(nil, "metaai", "Llama Future", []string{"meta-ai", "llama"}, func(_ *rod.Page, _ string) bool {
		return false
	})
	if ok {
		t.Fatal("unselectable non-default model should fail instead of silently using the default")
	}
}
