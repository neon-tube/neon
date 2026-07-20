package main;
use strict;
use warnings;

sub parse {
    my ($source) = @_;
    my ($ops, $pos) = parse_body($source, 0);
    return $ops;
}

sub parse_body {
    my ($source, $pos) = @_;
    my @acc;
    my $len = length($source);
    while ($pos < $len) {
        my $c = substr($source, $pos, 1);
        if ($c eq '+' || $c eq '-') {
            my $val = ($c eq '+') ? 1 : -1;
            $pos++;
            while ($pos < $len) {
                my $next_c = substr($source, $pos, 1);
                if ($next_c eq '+') { $val++; $pos++; }
                elsif ($next_c eq '-') { $val--; $pos++; }
                else { last; }
            }
            push @acc, [1, $val] if $val != 0; # 1 = Add
        } elsif ($c eq '>' || $c eq '<') {
            my $val = ($c eq '>') ? 1 : -1;
            $pos++;
            while ($pos < $len) {
                my $next_c = substr($source, $pos, 1);
                if ($next_c eq '>') { $val++; $pos++; }
                elsif ($next_c eq '<') { $val--; $pos++; }
                else { last; }
            }
            push @acc, [2, $val] if $val != 0; # 2 = Move
        } elsif ($c eq '.') {
            push @acc, [3, 0]; # 3 = Out
            $pos++;
        } elsif ($c eq ',') {
            push @acc, [4, 0]; # 4 = In
            $pos++;
        } elsif ($c eq '[') {
            my ($body, $next_pos) = parse_body($source, $pos + 1);
            push @acc, [5, $body]; # 5 = Loop
            $pos = $next_pos;
        } elsif ($c eq ']') {
            return (\@acc, $pos + 1);
        } else {
            $pos++;
        }
    }
    return (\@acc, $pos);
}

sub execute {
    my ($ops, $tape, $ptr) = @_;
    for my $op (@$ops) {
        my $type = $op->[0];
        my $val = $op->[1];
        if ($type == 1) {
            $tape->[$ptr] += $val;
        } elsif ($type == 2) {
            $ptr += $val;
        } elsif ($type == 3) {
            print $tape->[$ptr];
        } elsif ($type == 5) {
            while ($tape->[$ptr] != 0) {
                $ptr = execute($val, $tape, $ptr);
            }
        }
    }
    return $ptr;
}

sub main {
    my $program = '++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>++++++++++[>+<-]<-]<-]<-]<-]<-]<-]<-]';
    my $ops = parse($program);
    my @tape = (0) x 30000;
    execute($ops, \@tape, 0);
    print "Result: $tape[8]\n";
}

main();
