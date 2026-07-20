let pi = 3.141592653589793
let solar_mass = 4.0 *. pi *. pi
let days_per_year = 365.24
let n_bodies = 5

type body = {
  mutable x: float; mutable y: float; mutable z: float;
  mutable vx: float; mutable vy: float; mutable vz: float;
  mass: float;
}

let bodies = [|
  {
    x = 0.0; y = 0.0; z = 0.0;
    vx = 0.0; vy = 0.0; vz = 0.0;
    mass = solar_mass;
  };
  {
    x = 4.84143144246472090e+00; y = -1.16032004402742839e+00; z = -1.03622044471123109e-01;
    vx = 1.66007664274403694e-03 *. days_per_year;
    vy = 7.69901118419740425e-03 *. days_per_year;
    vz = -6.90460016972063023e-05 *. days_per_year;
    mass = 9.54791938424326609e-04 *. solar_mass;
  };
  {
    x = 8.34336671824457987e+00; y = 4.12479856412430479e+00; z = -4.03523417114321381e-01;
    vx = -2.76742510726862411e-03 *. days_per_year;
    vy = 4.99852801234917238e-03 *. days_per_year;
    vz = 2.30417297573763929e-05 *. days_per_year;
    mass = 2.85885980666130812e-04 *. solar_mass;
  };
  {
    x = 1.28943695621391310e+01; y = -1.51111514016986312e+01; z = -2.23307578892655734e-01;
    vx = 2.96460137564761618e-03 *. days_per_year;
    vy = 2.37847173959480950e-03 *. days_per_year;
    vz = -2.96589568540237556e-05 *. days_per_year;
    mass = 4.36624404335156298e-05 *. solar_mass;
  };
  {
    x = 1.53796971148509165e+01; y = -2.59193146099879641e+01; z = 1.79258772950371181e-01;
    vx = 2.68067772490389322e-03 *. days_per_year;
    vy = 1.62824170038242295e-03 *. days_per_year;
    vz = -9.51592254519715870e-05 *. days_per_year;
    mass = 5.15138902046611451e-05 *. solar_mass;
  }
|]

let offset_momentum () =
  let px = ref 0.0 and py = ref 0.0 and pz = ref 0.0 in
  for i = 0 to n_bodies - 1 do
    px := !px +. bodies.(i).vx *. bodies.(i).mass;
    py := !py +. bodies.(i).vy *. bodies.(i).mass;
    pz := !pz +. bodies.(i).vz *. bodies.(i).mass;
  done;
  bodies.(0).vx <- -. !px /. solar_mass;
  bodies.(0).vy <- -. !py /. solar_mass;
  bodies.(0).vz <- -. !pz /. solar_mass

let advance dt =
  for i = 0 to n_bodies - 1 do
    for j = i + 1 to n_bodies - 1 do
      let dx = bodies.(i).x -. bodies.(j).x in
      let dy = bodies.(i).y -. bodies.(j).y in
      let dz = bodies.(i).z -. bodies.(j).z in
      let d2 = dx *. dx +. dy *. dy +. dz *. dz in
      let mag = dt /. (d2 *. sqrt d2) in
      bodies.(i).vx <- bodies.(i).vx -. dx *. bodies.(j).mass *. mag;
      bodies.(i).vy <- bodies.(i).vy -. dy *. bodies.(j).mass *. mag;
      bodies.(i).vz <- bodies.(i).vz -. dz *. bodies.(j).mass *. mag;
      bodies.(j).vx <- bodies.(j).vx +. dx *. bodies.(i).mass *. mag;
      bodies.(j).vy <- bodies.(j).vy +. dy *. bodies.(i).mass *. mag;
      bodies.(j).vz <- bodies.(j).vz +. dz *. bodies.(i).mass *. mag;
    done
  done;
  for i = 0 to n_bodies - 1 do
    bodies.(i).x <- bodies.(i).x +. dt *. bodies.(i).vx;
    bodies.(i).y <- bodies.(i).y +. dt *. bodies.(i).vy;
    bodies.(i).z <- bodies.(i).z +. dt *. bodies.(i).vz;
  done

let energy () =
  let e = ref 0.0 in
  for i = 0 to n_bodies - 1 do
    e := !e +. 0.5 *. bodies.(i).mass *. (bodies.(i).vx *. bodies.(i).vx +. bodies.(i).vy *. bodies.(i).vy +. bodies.(i).vz *. bodies.(i).vz);
    for j = i + 1 to n_bodies - 1 do
      let dx = bodies.(i).x -. bodies.(j).x in
      let dy = bodies.(i).y -. bodies.(j).y in
      let dz = bodies.(i).z -. bodies.(j).z in
      e := !e -. (bodies.(i).mass *. bodies.(j).mass) /. sqrt (dx *. dx +. dy *. dy +. dz *. dz);
    done
  done;
  !e

let () =
  let n = 20000000 in
  offset_momentum ();
  let before = Printf.sprintf "%.9f" (energy ()) in
  Printf.printf "%s\n" before;
  for _ = 1 to n do
    advance 0.01
  done;
  let after = Printf.sprintf "%.9f" (energy ()) in
  Printf.printf "%s\n" after;
  Printf.printf "Result: %s %s\n" before after
