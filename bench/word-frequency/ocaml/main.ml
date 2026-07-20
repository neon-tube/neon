let fnv_offset_basis = 1469598103934665603L
let fnv_prime = 1099511628211L

let hash s =
  let h = ref fnv_offset_basis in
  for i = 0 to String.length s - 1 do
    h := Int64.logxor !h (Int64.of_int (Char.code s.[i]));
    h := Int64.mul !h fnv_prime
  done;
  !h

type slot = { mutable key: string option; mutable count: int }

let slots = ref (Array.make 0 { key = None; count = 0 })
let cap = ref 0
let used = ref 0

let rec grow () =
  let old_slots = !slots in
  let old_cap = !cap in
  cap := if old_cap = 0 then 16384 else old_cap * 2;
  slots := Array.init !cap (fun _ -> { key = None; count = 0 });
  used := 0;
  for i = 0 to old_cap - 1 do
    match old_slots.(i).key with
    | None -> ()
    | Some k ->
        let j = ref (Int64.to_int (Int64.logand (hash k) (Int64.of_int (!cap - 1)))) in
        while (!slots).(!j).key <> None do
          j := (!j + 1) land (!cap - 1)
        done;
        (!slots).(!j) <- { key = Some k; count = old_slots.(i).count };
        used := !used + 1
  done

let bump word =
  if !used * 10 >= !cap * 7 then grow ();
  let i = ref (Int64.to_int (Int64.logand (hash word) (Int64.of_int (!cap - 1)))) in
  let found = ref false in
  while (!slots).(!i).key <> None && not !found do
    match (!slots).(!i).key with
    | Some k when k = word ->
        (!slots).(!i).count <- (!slots).(!i).count + 1;
        found := true
    | _ ->
        i := (!i + 1) land (!cap - 1)
  done;
  if not !found then (
    (!slots).(!i) <- { key = Some word; count = 1 };
    used := !used + 1
  )

let () =
  let x = ref 42L in
  let n = 10000000 in
  for _ = 1 to n do
    x := Int64.rem (Int64.mul !x 48271L) 2147483647L;
    let word = Printf.sprintf "w%Ld" (Int64.rem !x 10000L) in
    bump word
  done;
  let max_val = ref 0 in
  let distinct = ref 0 in
  for i = 0 to !cap - 1 do
    if (!slots).(i).key <> None then (
      distinct := !distinct + 1;
      if (!slots).(i).count > !max_val then max_val := (!slots).(i).count
    )
  done;
  Printf.printf "Result: %d %d %d\n" !distinct n !max_val
