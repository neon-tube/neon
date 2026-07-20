module Main where

import Data.Bits (shiftL)

data Tree = Node Tree Tree | Nil

make :: Int -> Tree
make 0 = Node Nil Nil
make d = Node (make (d - 1)) (make (d - 1))

check :: Tree -> Int
check Nil = 0
check (Node l r) = 1 + check l + check r

main :: IO ()
main = do
    let max_depth = 18
    let stretch = make (max_depth + 1)
    let sc = check stretch
    putStrLn $ "stretch tree of depth " ++ show (max_depth + 1) ++ " check: " ++ show sc
    
    let long_lived = make max_depth
    
    let loop depth total | depth > max_depth = return total
                         | otherwise = do
            let iterations = 1 `shiftL` (max_depth - depth + 4) :: Int
            let sum' = sum [check (make depth) | _ <- [1..iterations]]
            putStrLn $ show iterations ++ " trees of depth " ++ show depth ++ " check: " ++ show sum'
            loop (depth + 2) (total + sum')
            
    total1 <- loop 4 sc
    let ll = check long_lived
    putStrLn $ "long lived tree of depth " ++ show max_depth ++ " check: " ++ show ll
    putStrLn $ "Result: " ++ show (total1 + ll)
