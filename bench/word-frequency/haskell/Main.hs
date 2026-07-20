module Main where

import Data.Int (Int64)
import Data.Array.IO
import Control.Monad
import Data.Bits
import Data.Char (ord)
import Text.Printf

hash :: String -> Int64
hash s = foldl (\h c -> (h `xor` fromIntegral (ord c)) * 1099511628211) 1469598103934665603 s

data Table = Table { cap :: Int, used :: Int, keys :: IOArray Int String, counts :: IOUArray Int Int64 }

newTable :: Int -> IO Table
newTable c = do
  k <- newArray (0, c - 1) ""
  v <- newArray (0, c - 1) 0
  return $ Table c 0 k v

grow :: Table -> IO Table
grow t = do
  let c = if cap t == 0 then 16384 else cap t * 2
  nt <- newTable c
  when (cap t > 0) $ do
    forM_ [0..cap t - 1] $ \i -> do
      k <- readArray (keys t) i
      when (k /= "") $ do
        v <- readArray (counts t) i
        insert nt k v
  return nt

insert :: Table -> String -> Int64 -> IO ()
insert t k v = do
  let h = fromIntegral (hash k) .&. (cap t - 1)
  let loop i = do
        ex_k <- readArray (keys t) i
        if ex_k == "" then do
          writeArray (keys t) i k
          writeArray (counts t) i v
        else loop ((i + 1) .&. (cap t - 1))
  loop h

bump :: Table -> String -> IO Table
bump t k = do
  t' <- if used t * 10 >= cap t * 7 then grow t else return t
  let h = fromIntegral (hash k) .&. (cap t' - 1)
  let loop i = do
        ex_k <- readArray (keys t') i
        if ex_k == "" then do
          writeArray (keys t') i k
          writeArray (counts t') i 1
          return t' { used = used t' + 1 }
        else if ex_k == k then do
          ex_v <- readArray (counts t') i
          writeArray (counts t') i (ex_v + 1)
          return t'
        else loop ((i + 1) .&. (cap t' - 1))
  loop h

main :: IO ()
main = do
  t_ref <- newTable 0 >>= grow >>= (\t -> newArray (0,0) t :: IO (IOArray Int Table))
  
  x_ref <- newArray (0,0) 42 :: IO (IOUArray Int Int64)
  let n = 10000000 :: Int64
  
  forM_ [1..n] $ \_ -> do
    x <- readArray x_ref 0
    let nx = (x * 48271) `mod` 2147483647
    writeArray x_ref 0 nx
    let word = "w" ++ show (nx `mod` 10000)
    t <- readArray t_ref 0
    t' <- bump t word
    writeArray t_ref 0 t'
    
  t <- readArray t_ref 0
  max_ref <- newArray (0,0) 0 :: IO (IOUArray Int Int64)
  dist_ref <- newArray (0,0) 0 :: IO (IOUArray Int Int)
  
  forM_ [0..cap t - 1] $ \i -> do
    k <- readArray (keys t) i
    when (k /= "") $ do
      d <- readArray dist_ref 0
      writeArray dist_ref 0 (d + 1)
      v <- readArray (counts t) i
      m <- readArray max_ref 0
      when (v > m) $ writeArray max_ref 0 v
      
  d <- readArray dist_ref 0
  m <- readArray max_ref 0
  printf "Result: %d %d %d\n" d n m
