package providers

import "testing"

func TestNormalizeModelOption(t *testing.T) {
	tests := []struct {
		raw  string
		want string
	}{
		{raw: "Claude 3.7 Sonnet\nFast reasoning model", want: "Claude 3.7 Sonnet"},
		{raw: "  Qwen   Coder  ", want: "Qwen Coder"},
		{raw: "Selected\nGPT-5", want: "GPT-5"},
		{raw: "Settings", want: ""},
	}
	for _, test := range tests {
		if got := normalizeModelOption(test.raw); got != test.want {
			t.Errorf("normalizeModelOption(%q) = %q, want %q", test.raw, got, test.want)
		}
	}
}
