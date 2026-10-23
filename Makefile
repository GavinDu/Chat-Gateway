BINARY  := go-proxy
PKG     := go-proxy
PORT    ?= 8080

.PHONY: build run clean tidy login help

## build: compile the binary
build:
	go build -o $(BINARY) .

## run: build and start the server
run: build
	PORT=$(PORT) ./$(BINARY) --config config.yaml

## tidy: tidy and download Go modules
tidy:
	go mod tidy

## login: open a headed browser for manual Qwen or Meta AI authentication
##   Usage: make login PROVIDER=qwen
login: build
	./$(BINARY) --login $(PROVIDER) --config config.yaml

## clean: remove compiled binary
clean:
	rm -f $(BINARY)

## help: show this help
help:
	@grep -E '^##' Makefile | sed 's/## //'
