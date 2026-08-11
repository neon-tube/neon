{-# LANGUAGE OverloadedStrings #-}
{-# LANGUAGE ScopedTypeVariables #-}

-- http-datetime: Haskell, the `network` + `time` packages.
-- forkIO a lightweight thread per accepted connection; HTTP/1.1 keep-alive loop.
module Main (main) where

import Control.Concurrent (forkIO)
import Control.Exception (SomeException, finally, try)
import Control.Monad (forever, void)
import Data.Char (toLower)
import Data.Time (defaultTimeLocale, formatTime, getCurrentTime)
import Network.Socket
import System.Environment (lookupEnv)

import qualified Data.ByteString as BS
import qualified Data.ByteString.Char8 as BC
import qualified Network.Socket.ByteString as NSB

main :: IO ()
main = withSocketsDo $ do
  port <- maybe "18080" id <$> lookupEnv "PORT"
  let hints = defaultHints { addrFlags = [AI_PASSIVE], addrSocketType = Stream }
  addr <- head <$> getAddrInfo (Just hints) (Just "127.0.0.1") (Just port)
  sock <- socket (addrFamily addr) (addrSocketType addr) (addrProtocol addr)
  setSocketOption sock ReuseAddr 1
  bind sock (addrAddress addr)
  listen sock 1024
  forever $ do
    (conn, _) <- accept sock
    -- A bad connection is dropped, never the whole server.
    void $ forkIO $ (serve conn `finally` close conn)

serve :: Socket -> IO ()
serve conn = void (try loop :: IO (Either SomeException ()))
  where
    loop = go BS.empty
    go buf = do
      mreq <- readRequest buf
      case mreq of
        Nothing -> pure ()                     -- EOF: client hung up
        Just (headers, rest) -> do
          now <- getCurrentTime
          let body = BC.pack (formatTime defaultTimeLocale "%Y-%m-%dT%H:%M:%SZ" now)
              keep = not (wantsClose headers)
              connHdr = if keep then "keep-alive" else "close"
              resp = BS.concat
                [ "HTTP/1.1 200 OK\r\n"
                , "Content-Type: text/plain\r\n"
                , "Content-Length: ", BC.pack (show (BS.length body)), "\r\n"
                , "Connection: ", connHdr, "\r\n\r\n"
                , body ]
          NSB.sendAll conn resp
          if keep then go rest else pure ()

    -- Accumulate bytes until the "\r\n\r\n" header terminator; keep the leftover.
    readRequest buf =
      let (pre, post) = BS.breakSubstring "\r\n\r\n" buf
      in if not (BS.null post)
           then pure (Just (BS.take (BS.length pre + 4) buf, BS.drop (BS.length pre + 4) buf))
           else do
             chunk <- NSB.recv conn 4096
             if BS.null chunk
               then pure Nothing
               else readRequest (buf <> chunk)

wantsClose :: BS.ByteString -> Bool
wantsClose headers = "connection: close" `BS.isInfixOf` BC.map toLower headers
