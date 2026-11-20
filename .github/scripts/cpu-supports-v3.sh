#!/bin/bash
# Exit 0 when the CPU of this Linux x86-64 machine has every extension the x86-64-v3 toolchain build
# uses, and print what is missing and exit 1 otherwise. The list is the one frust-install checks
# (crates/frust-cli/src/release.rs): the x86-64-v2 set plus AVX, AVX2, BMI1, BMI2, FMA, MOVBE, LZCNT
# (abm in /proc/cpuinfo), F16C and XSAVE.
# Usage: cpu-supports-v3.sh
set -euo pipefail
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
    echo "not a Linux x86-64 machine" >&2
    exit 1
fi
flags=$(awk -F': *' '$1 ~ /^flags/ { print $2; exit }' /proc/cpuinfo)
model=$(awk -F': *' '$1 ~ /^model name/ { print $2; exit }' /proc/cpuinfo)
missing=
for flag in cx16 lahf_lm popcnt pni ssse3 sse4_1 sse4_2 avx avx2 bmi1 bmi2 fma movbe abm f16c xsave; do
    case " $flags " in
        *" $flag "*) ;;
        *) missing="$missing $flag" ;;
    esac
done
if [[ -n $missing ]]; then
    echo "$model lacks x86-64-v3 extensions:$missing"
    exit 1
fi
echo "$model supports x86-64-v3"
