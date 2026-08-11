package main

import (
	"io"
	"net/http"
	"os"
	"time"
)

func handler(w http.ResponseWriter, r *http.Request) {
	// Drain and discard any request body so the connection stays reusable.
	io.Copy(io.Discard, r.Body)
	now := time.Now().UTC().Format(time.RFC3339)
	w.Header().Set("Content-Type", "text/plain")
	w.Write([]byte(now))
}

func main() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "18080"
	}
	srv := &http.Server{
		Addr:    "127.0.0.1:" + port,
		Handler: http.HandlerFunc(handler),
	}
	if err := srv.ListenAndServe(); err != nil {
		panic(err)
	}
}
