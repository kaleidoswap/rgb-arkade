// arkdb: test tool simulating a *colluding operator* that edits its own arkd sqlite DB.
// usage: arkdb <db> exec "<sql>" | arkdb <db> query "<sql>"
package main

import (
	"database/sql"
	"fmt"
	"os"
	"strings"

	_ "modernc.org/sqlite"
)

func main() {
	db, err := sql.Open("sqlite", os.Args[1]+"?_pragma=busy_timeout(10000)")
	if err != nil {
		panic(err)
	}
	defer db.Close()
	switch os.Args[2] {
	case "exec":
		r, err := db.Exec(os.Args[3])
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		n, _ := r.RowsAffected()
		fmt.Println("rows affected", n)
	case "query":
		rows, err := db.Query(os.Args[3])
		if err != nil {
			fmt.Println("ERR", err)
			os.Exit(1)
		}
		cols, _ := rows.Columns()
		fmt.Println(strings.Join(cols, "|"))
		for rows.Next() {
			vals := make([]any, len(cols))
			ptrs := make([]any, len(cols))
			for i := range vals {
				ptrs[i] = &vals[i]
			}
			rows.Scan(ptrs...)
			s := make([]string, len(cols))
			for i, v := range vals {
				s[i] = fmt.Sprint(v)
			}
			fmt.Println(strings.Join(s, "|"))
		}
	}
}
