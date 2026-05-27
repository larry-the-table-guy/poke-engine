# Takes 2 bench binaries, compares performance, produces report for GitHub PR.
# poor error handling, but idk
# USAGE:
# sh ./benches/compare.sh ./old-bench-bin ./new-bench-bin 5 diff.full.md 0 1 2 4

if test "$#" -lt 5; then
    echo "expects 5+ parameters: 'old-bench-bin' 'new-bench-bin' 'time' 'out-file' 'thread counts...'"
    exit 1
fi

mkdir -p ./benches/reports

oldbin="$1"
newbin="$2"
time="$3"
outpath="$4"
shift 4

for n in "$@"; do
    echo "running old w/ $n";
    cat ./benches/states/gen9.t0.example.txt | "$oldbin" bench --time=$time --threads=$n > "./benches/reports/old.$n.report"
    echo "running new w/ $n"
    cat ./benches/states/gen9.t0.example.txt | "$newbin" bench --time=$time --threads=$n > "./benches/reports/new.$n.report"
done

(
 for n in "$@"; do $newbin diff --short --title="Old vs New; threads: $n" "./benches/reports/old.$n.report" "./benches/reports/new.$n.report"; done
 if test "$#" -ne 1; then
	 tmpstr="$1"
	 reportlist=""
	 for s in "$@"; do reportlist="$reportlist ./benches/reports/new.$s.report"; done
	 shift 1
	 for s in "$@"; do tmpstr="$tmpstr vs $s"; done
	 $newbin diff --short --title="New; threads: $tmpstr" $reportlist
 fi
) > $outpath

