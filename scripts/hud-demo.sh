#!/bin/zsh
# Shows the progress panel with sample rows, without switching anything.
bin=${1:-$HOME/bin/monitor-switch}
{
  printf 'row\ttitle\tSwitching to Mac mini\tbusy\t\n'
  printf 'row\tk\tMagic Keyboard\tbusy\t\n'
  printf 'row\tm\tMagic Mouse\tbusy\t\n'
  sleep 2
  printf 'row\ttitle\tSwitched to Mac mini\tok\t\n'
  sleep 2
  printf 'row\tk\t\tok\t\n'
  sleep 3
  printf 'row\tm\t\tbusy\tNot answering. Click it, or switch it off and on\n'
  sleep 3
  printf 'row\tm\t\tfail\tPairing failed (timed out)\n'
  printf 'error\tMagic Mouse: pairing failed (timed out)\n'
  printf 'close\t5\n'
  sleep 6
} | "$bin" hud
