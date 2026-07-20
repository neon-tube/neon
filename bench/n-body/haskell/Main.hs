module Main where

import Data.Array.IO
import Control.Monad
import Text.Printf (printf)

n_bodies = 5
solar_mass = 4.0 * pi * pi
days_per_year = 365.24

main :: IO ()
main = do
  bodies <- newListArray (0, 34) [
    0, 0, 0, 0, 0, 0, solar_mass,
    4.84143144246472090e+00, -1.16032004402742839e+00, -1.03622044471123109e-01,
    1.66007664274403694e-03 * days_per_year, 7.69901118419740425e-03 * days_per_year, -6.90460016972063023e-05 * days_per_year, 9.54791938424326609e-04 * solar_mass,
    8.34336671824457987e+00, 4.12479856412430479e+00, -4.03523417114321381e-01,
    -2.76742510726862411e-03 * days_per_year, 4.99852801234917238e-03 * days_per_year, 2.30417297573763929e-05 * days_per_year, 2.85885980666130812e-04 * solar_mass,
    1.28943695621391310e+01, -1.51111514016986312e+01, -2.23307578892655734e-01,
    2.96460137564761618e-03 * days_per_year, 2.37847173959480950e-03 * days_per_year, -2.96589568540237556e-05 * days_per_year, 4.36624404335156298e-05 * solar_mass,
    1.53796971148509165e+01, -2.59193146099879641e+01, 1.79258772950371181e-01,
    2.68067772490389322e-03 * days_per_year, 1.62824170038242295e-03 * days_per_year, -9.51592254519715870e-05 * days_per_year, 5.15138902046611451e-05 * solar_mass
    ] :: IO (IOUArray Int Double)
    
  let offset_momentum = do
        px <- newArray (0,0) 0 :: IO (IOUArray Int Double)
        py <- newArray (0,0) 0 :: IO (IOUArray Int Double)
        pz <- newArray (0,0) 0 :: IO (IOUArray Int Double)
        forM_ [0..4] $ \i -> do
          vx <- readArray bodies (i*7 + 3)
          vy <- readArray bodies (i*7 + 4)
          vz <- readArray bodies (i*7 + 5)
          mass <- readArray bodies (i*7 + 6)
          px_val <- readArray px 0
          writeArray px 0 (px_val + vx * mass)
          py_val <- readArray py 0
          writeArray py 0 (py_val + vy * mass)
          pz_val <- readArray pz 0
          writeArray pz 0 (pz_val + vz * mass)
        px_final <- readArray px 0
        py_final <- readArray py 0
        pz_final <- readArray pz 0
        writeArray bodies 3 (-px_final / solar_mass)
        writeArray bodies 4 (-py_final / solar_mass)
        writeArray bodies 5 (-pz_final / solar_mass)

  let advance dt = do
        forM_ [0..4] $ \i -> do
          forM_ [i+1..4] $ \j -> do
            xi <- readArray bodies (i*7 + 0)
            yi <- readArray bodies (i*7 + 1)
            zi <- readArray bodies (i*7 + 2)
            xj <- readArray bodies (j*7 + 0)
            yj <- readArray bodies (j*7 + 1)
            zj <- readArray bodies (j*7 + 2)
            let dx = xi - xj
                dy = yi - yj
                dz = zi - zj
                d2 = dx*dx + dy*dy + dz*dz
                mag = dt / (d2 * sqrt d2)
            massi <- readArray bodies (i*7 + 6)
            massj <- readArray bodies (j*7 + 6)
            vxi <- readArray bodies (i*7 + 3)
            vyi <- readArray bodies (i*7 + 4)
            vzi <- readArray bodies (i*7 + 5)
            vxj <- readArray bodies (j*7 + 3)
            vyj <- readArray bodies (j*7 + 4)
            vzj <- readArray bodies (j*7 + 5)
            writeArray bodies (i*7 + 3) (vxi - dx * massj * mag)
            writeArray bodies (i*7 + 4) (vyi - dy * massj * mag)
            writeArray bodies (i*7 + 5) (vzi - dz * massj * mag)
            writeArray bodies (j*7 + 3) (vxj + dx * massi * mag)
            writeArray bodies (j*7 + 4) (vyj + dy * massi * mag)
            writeArray bodies (j*7 + 5) (vzj + dz * massi * mag)
        forM_ [0..4] $ \i -> do
          xi <- readArray bodies (i*7 + 0)
          yi <- readArray bodies (i*7 + 1)
          zi <- readArray bodies (i*7 + 2)
          vxi <- readArray bodies (i*7 + 3)
          vyi <- readArray bodies (i*7 + 4)
          vzi <- readArray bodies (i*7 + 5)
          writeArray bodies (i*7 + 0) (xi + dt * vxi)
          writeArray bodies (i*7 + 1) (yi + dt * vyi)
          writeArray bodies (i*7 + 2) (zi + dt * vzi)

  let energy = do
        e <- newArray (0,0) 0 :: IO (IOUArray Int Double)
        forM_ [0..4] $ \i -> do
          mass <- readArray bodies (i*7 + 6)
          vx <- readArray bodies (i*7 + 3)
          vy <- readArray bodies (i*7 + 4)
          vz <- readArray bodies (i*7 + 5)
          eval <- readArray e 0
          writeArray e 0 (eval + 0.5 * mass * (vx*vx + vy*vy + vz*vz))
          forM_ [i+1..4] $ \j -> do
            xi <- readArray bodies (i*7 + 0)
            yi <- readArray bodies (i*7 + 1)
            zi <- readArray bodies (i*7 + 2)
            xj <- readArray bodies (j*7 + 0)
            yj <- readArray bodies (j*7 + 1)
            zj <- readArray bodies (j*7 + 2)
            massj <- readArray bodies (j*7 + 6)
            let dx = xi - xj
                dy = yi - yj
                dz = zi - zj
            eval' <- readArray e 0
            writeArray e 0 (eval' - (mass * massj) / sqrt (dx*dx + dy*dy + dz*dz))
        readArray e 0
        
  offset_momentum
  e1 <- energy
  
  let loop (0 :: Int) = return ()
      loop k = advance 0.01 >> loop (k - 1)
  loop 20000000
  e2 <- energy
  
  let before = printf "%.9f" e1 :: String
  let after = printf "%.9f" e2 :: String
  putStrLn before
  putStrLn after
  printf "Result: %s %s\n" before after
