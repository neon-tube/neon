(* http-datetime: OCaml, Unix module only.
   fork() a child per accepted connection; HTTP/1.1 keep-alive loop in the child. *)

let port =
  match Sys.getenv_opt "PORT" with
  | Some s -> (try int_of_string s with _ -> 18080)
  | None -> 18080

(* Current UTC time as YYYY-MM-DDTHH:MM:SSZ, computed per request. *)
let now_iso () =
  let t = Unix.gmtime (Unix.time ()) in
  Printf.sprintf "%04d-%02d-%02dT%02d:%02d:%02dZ"
    (t.Unix.tm_year + 1900) (t.Unix.tm_mon + 1) t.Unix.tm_mday
    t.Unix.tm_hour t.Unix.tm_min t.Unix.tm_sec

(* Index of the first "\r\n\r\n" in s, or -1. *)
let find_terminator s =
  let n = String.length s in
  let rec go i =
    if i + 4 > n then -1
    else if s.[i] = '\r' && s.[i+1] = '\n' && s.[i+2] = '\r' && s.[i+3] = '\n' then i
    else go (i + 1)
  in
  go 0

(* Does the header block ask for the connection to close? *)
let wants_close headers =
  let lines = String.split_on_char '\n' headers in
  List.exists
    (fun line ->
      let line = String.lowercase_ascii (String.trim line) in
      let p = "connection:" in
      let pl = String.length p in
      String.length line >= pl
      && String.sub line 0 pl = p
      && String.trim (String.sub line pl (String.length line - pl)) = "close")
    lines

let write_all fd s =
  let len = String.length s in
  let rec go off =
    if off < len then
      let w = Unix.write_substring fd s off (len - off) in
      go (off + w)
  in
  go 0

let handle client =
  let pending = ref "" in
  let readbuf = Bytes.create 4096 in
  (* Read one request: bytes through the \r\n\r\n terminator. Leftover bytes stay
     in [pending] so pipelined requests on the same connection are not lost. *)
  let rec read_request () =
    let idx = find_terminator !pending in
    if idx >= 0 then begin
      let headers = String.sub !pending 0 (idx + 4) in
      pending := String.sub !pending (idx + 4) (String.length !pending - idx - 4);
      Some headers
    end
    else begin
      let n = Unix.read client readbuf 0 (Bytes.length readbuf) in
      if n = 0 then None (* EOF: client closed *)
      else begin
        pending := !pending ^ Bytes.sub_string readbuf 0 n;
        read_request ()
      end
    end
  in
  let rec loop () =
    match read_request () with
    | None -> ()
    | Some headers ->
      let now = now_iso () in
      let keep = not (wants_close headers) in
      let conn = if keep then "keep-alive" else "close" in
      let resp =
        Printf.sprintf
          "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: %d\r\nConnection: %s\r\n\r\n%s"
          (String.length now) conn now
      in
      write_all client resp;
      if keep then loop ()
  in
  (try loop () with _ -> ());
  (try Unix.close client with _ -> ())

let () =
  Sys.set_signal Sys.sigpipe Sys.Signal_ignore;
  (* Reap children so they never become zombies. *)
  Sys.set_signal Sys.sigchld
    (Sys.Signal_handle
       (fun _ ->
         try
           while fst (Unix.waitpid [ Unix.WNOHANG ] (-1)) > 0 do
             ()
           done
         with Unix.Unix_error (Unix.ECHILD, _, _) -> ()));
  let sock = Unix.socket Unix.PF_INET Unix.SOCK_STREAM 0 in
  Unix.setsockopt sock Unix.SO_REUSEADDR true;
  Unix.bind sock (Unix.ADDR_INET (Unix.inet_addr_loopback, port));
  Unix.listen sock 1024;
  let rec accept_loop () =
    (match
       try Some (fst (Unix.accept sock))
       with Unix.Unix_error (Unix.EINTR, _, _) -> None (* signal woke accept *)
     with
    | None -> ()
    | Some client -> (
      match Unix.fork () with
      | 0 ->
        Unix.close sock;
        handle client;
        exit 0
      | _ -> ( try Unix.close client with _ -> ())
      | exception Unix.Unix_error _ -> ( try Unix.close client with _ -> ())));
    accept_loop ()
  in
  accept_loop ()
