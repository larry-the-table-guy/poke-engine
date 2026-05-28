# Takes 2 bench binaries, compares performance, produces report for GitHub PR.
# poor error handling, but idk
# USAGE:
# sh ./benches/compare.sh ./data/gen9states.txt ./old-bench-bin ./new-bench-bin 5 diff.md 0 1 2 4

if test "$#" -lt 6; then
    echo "expects 6+ parameters: 'input-file' 'old-bench-bin' 'new-bench-bin' 'time' 'out-file' 'thread counts...'"
    exit 1
fi

mkdir -p ./benches/reports

inputpath="$1"
oldbin="$2"
newbin="$3"
time="$4"
outpath="$5"
shift 5

for n in "$@"; do
    echo "running old w/ $n";
    cat "$inputpath" | "$oldbin" bench --time=$time --threads=$n > "./benches/reports/old.$n.report"
    echo "running new w/ $n"
    cat "$inputpath" | "$newbin" bench --time=$time --threads=$n > "./benches/reports/new.$n.report"
done

(
 for n in "$@"; do $newbin diff --skip-identical --short --title="Old vs New; threads: $n" "./benches/reports/old.$n.report" "./benches/reports/new.$n.report"; done
 if test "$#" -ne 1; then
	 tmpstr="$1"
	 reportlist=""
	 for s in "$@"; do reportlist="$reportlist ./benches/reports/new.$s.report"; done
	 shift 1
	 for s in "$@"; do tmpstr="$tmpstr vs $s"; done
	 $newbin diff --skip-identical --short --title="New; threads: $tmpstr" $reportlist
 fi
) > $outpath

