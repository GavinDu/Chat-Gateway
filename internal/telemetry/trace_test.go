package telemetry

import (
	"context"
	"testing"
	"time"
)

func TestAttemptTraceRecordsMarksOnceAndFlags(t *testing.T) {
	ctx, trace := WithAttempt(context.Background())
	Mark(ctx, "browser_acquired")
	time.Sleep(time.Millisecond)
	Mark(ctx, "lock_acquired")
	Mark(ctx, "lock_acquired")
	SetFlags(ctx, func(flags *Flags) {
		flags.ReusedBrowser = true
		flags.NewPage = true
	})

	snapshot := trace.Snapshot()
	if !snapshot.Flags.ReusedBrowser || !snapshot.Flags.NewPage {
		t.Fatalf("flags not retained: %#v", snapshot.Flags)
	}
	if snapshot.ProviderQueueMs < 1 {
		t.Fatalf("provider queue duration not recorded: %d", snapshot.ProviderQueueMs)
	}
	if snapshot.AttemptTotalMs < snapshot.ProviderQueueMs {
		t.Fatalf("total %d shorter than queue %d", snapshot.AttemptTotalMs, snapshot.ProviderQueueMs)
	}
}

func TestResponseActivityIsAggregated(t *testing.T) {
	ctx, trace := WithAttempt(context.Background())
	Mark(ctx, "request_sent")
	ObserveResponseUpdate(ctx)
	time.Sleep(time.Millisecond)
	ObserveResponseUpdate(ctx)
	Mark(ctx, "response_stable")
	Mark(ctx, "complete")

	snapshot := trace.Snapshot()
	if snapshot.ResponseUpdateCount != 2 {
		t.Fatalf("response update count = %d, want 2", snapshot.ResponseUpdateCount)
	}
	if snapshot.ActiveGenerationMs < 1 {
		t.Fatalf("active generation duration not recorded: %d", snapshot.ActiveGenerationMs)
	}
}

func TestMarksWithoutTraceAreNoops(t *testing.T) {
	ctx := context.Background()
	Mark(ctx, "first_token")
	SetFlags(ctx, func(flags *Flags) {
		flags.ReusedPage = true
	})
}
