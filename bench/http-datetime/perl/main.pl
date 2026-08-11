#!/usr/bin/perl
# http-datetime: Perl, core modules only (IO::Socket::INET, POSIX).
# fork() a child per accepted connection, HTTP/1.1 keep-alive loop.
use strict;
use warnings;
use IO::Socket::INET;
use POSIX qw(strftime :sys_wait_h);

$| = 1;
$SIG{PIPE} = 'IGNORE';                 # writing to a hung-up peer must not kill us
$SIG{CHLD} = sub { while (waitpid(-1, WNOHANG) > 0) {} };   # reap children

my $port = $ENV{PORT} // 18080;

my $server = IO::Socket::INET->new(
    LocalAddr => '127.0.0.1',
    LocalPort => $port,
    Proto     => 'tcp',
    Listen    => SOMAXCONN,
    ReuseAddr => 1,
) or die "cannot bind 127.0.0.1:$port: $!\n";

while (1) {
    my $client = $server->accept or next;   # EINTR from SIGCHLD -> retry
    my $pid = fork();
    if (!defined $pid) {
        $client->close;                     # out of resources: drop this conn
        next;
    }
    if ($pid == 0) {
        $server->close;
        handle($client);
        exit 0;
    }
    $client->close;                         # parent keeps listening
}

sub handle {
    my ($sock) = @_;
    $sock->autoflush(1);
    my $buf = '';
    while (1) {
        # Read one request: consume through the "\r\n\r\n" header terminator.
        while (index($buf, "\r\n\r\n") < 0) {
            my $n = sysread($sock, my $chunk, 4096);
            last unless defined $n;         # error
            return if $n == 0;              # EOF: client closed
            $buf .= $chunk;
        }
        my $idx = index($buf, "\r\n\r\n");
        last if $idx < 0;
        my $headers = substr($buf, 0, $idx + 4, '');   # remove parsed request

        my $now = strftime("%Y-%m-%dT%H:%M:%SZ", gmtime);
        my $keep = $headers !~ /^connection:\s*close/mi;
        my $conn = $keep ? "keep-alive" : "close";
        my $len = length($now);

        my $resp = "HTTP/1.1 200 OK\r\n"
                 . "Content-Type: text/plain\r\n"
                 . "Content-Length: $len\r\n"
                 . "Connection: $conn\r\n"
                 . "\r\n"
                 . $now;
        my $written = syswrite($sock, $resp);
        return unless defined $written;     # peer gone
        last unless $keep;
    }
    $sock->close;
}
