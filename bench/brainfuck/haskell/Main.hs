module Main where

import Data.Array.IO
import Control.Monad

data Op = OpAdd Int | OpMove Int | OpOut | OpIn | OpLoop [Op] deriving Show

parse :: String -> [Op]
parse source = fst $ parseBody source 0
  where
    len = length source
    parseBody :: String -> Int -> ([Op], Int)
    parseBody s pos
      | pos >= len = ([], pos)
      | otherwise = 
          let c = s !! pos in
          case c of
            '+' -> parseAdd s pos 0
            '-' -> parseAdd s pos 0
            '>' -> parseMove s pos 0
            '<' -> parseMove s pos 0
            '.' -> let (acc, npos) = parseBody s (pos + 1) in (OpOut : acc, npos)
            ',' -> let (acc, npos) = parseBody s (pos + 1) in (OpIn : acc, npos)
            '[' -> let (body, npos) = parseBody s (pos + 1)
                       (acc, nnpos) = parseBody s npos
                   in (OpLoop body : acc, nnpos)
            ']' -> ([], pos + 1)
            _   -> parseBody s (pos + 1)
    
    parseAdd s p v
      | p < len && s !! p == '+' = parseAdd s (p + 1) (v + 1)
      | p < len && s !! p == '-' = parseAdd s (p + 1) (v - 1)
      | otherwise = let (acc, npos) = parseBody s p
                    in if v /= 0 then (OpAdd v : acc, npos) else (acc, npos)
                    
    parseMove s p v
      | p < len && s !! p == '>' = parseMove s (p + 1) (v + 1)
      | p < len && s !! p == '<' = parseMove s (p + 1) (v - 1)
      | otherwise = let (acc, npos) = parseBody s p
                    in if v /= 0 then (OpMove v : acc, npos) else (acc, npos)

execute :: [Op] -> IO Int
execute ops = do
  tape <- newArray (0, 29999) 0 :: IO (IOUArray Int Int)
  ptr <- newArray (0, 0) 0 :: IO (IOUArray Int Int)
  let exec [] = return ()
      exec (OpAdd v : rest) = do
        p <- readArray ptr 0
        val <- readArray tape p
        writeArray tape p (val + v)
        exec rest
      exec (OpMove v : rest) = do
        p <- readArray ptr 0
        writeArray ptr 0 (p + v)
        exec rest
      exec (OpOut : rest) = do
        p <- readArray ptr 0
        val <- readArray tape p
        putStr (show val)
        exec rest
      exec (OpIn : rest) = exec rest
      exec (OpLoop body : rest) = do
        let loop = do
              p <- readArray ptr 0
              val <- readArray tape p
              if val /= 0 then exec body >> loop else return ()
        loop
        exec rest
  exec ops
  readArray tape 8

main :: IO ()
main = do
  let program = "++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]"
  let ops = parse program
  res <- execute ops
  putStrLn $ "Result: " ++ show res
