;; http-datetime: Clojure, Java interop only (java.net.ServerSocket, java.time).
;; A thread per accepted connection, HTTP/1.1 keep-alive loop over the socket streams.
(ns main
  (:import [java.net ServerSocket InetAddress Socket]
           [java.io InputStream OutputStream]
           [java.time Instant]
           [java.time.format DateTimeFormatter]
           [java.nio.charset StandardCharsets]))

(def ^DateTimeFormatter iso DateTimeFormatter/ISO_INSTANT)

(defn now-iso ^String []
  ;; ISO_INSTANT on a whole-second Instant -> "2026-08-11T17:30:00Z".
  (.format iso (.truncatedTo (Instant/now) java.time.temporal.ChronoUnit/SECONDS)))

(defn read-request
  "Consume bytes from `in` through the \\r\\n\\r\\n header terminator.
   Returns the header string, or nil on EOF/error."
  [^InputStream in]
  (let [sb (StringBuilder.)]
    (loop [state 0]                     ; count of consecutive terminator chars matched
      (let [b (.read in)]
        (if (neg? b)
          (when (pos? (.length sb)) (.toString sb))   ; EOF: nil if nothing read
          (do
            (.append sb (char b))
            (let [state' (cond
                           (and (== state 0) (== b 13)) 1   ; \r
                           (and (== state 1) (== b 10)) 2   ; \n
                           (and (== state 2) (== b 13)) 3   ; \r
                           (and (== state 3) (== b 10)) 4   ; \n
                           (== b 13) 1
                           :else 0)]
              (if (== state' 4)
                (.toString sb)
                (recur state')))))))))

(defn handle [^Socket sock]
  (try
    (let [in  (.getInputStream sock)
          out (.getOutputStream sock)]
      (loop []
        (let [headers (read-request in)]
          (when (and headers (pos? (.length ^String headers)))
            (let [now  (now-iso)
                  keep (not (re-find #"(?im)^connection:\s*close" headers))
                  conn (if keep "keep-alive" "close")
                  resp (str "HTTP/1.1 200 OK\r\n"
                            "Content-Type: text/plain\r\n"
                            "Content-Length: " (count (.getBytes now StandardCharsets/UTF_8)) "\r\n"
                            "Connection: " conn "\r\n"
                            "\r\n"
                            now)]
              (.write out (.getBytes resp StandardCharsets/UTF_8))
              (.flush out)
              (when keep (recur)))))))
    (catch Exception _ nil)             ; a bad connection is dropped, not the server
    (finally (try (.close sock) (catch Exception _ nil)))))

(defn -main [& _]
  (let [port (Integer/parseInt (or (System/getenv "PORT") "18080"))
        server (ServerSocket. port 1024 (InetAddress/getByName "127.0.0.1"))]
    (loop []
      (let [client (.accept server)]
        (.start (Thread. ^Runnable (fn [] (handle client)))))
      (recur))))

(-main)
