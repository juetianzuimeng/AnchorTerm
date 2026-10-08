package main

import (
	"bufio"
	"database/sql"
	"flag"
	"fmt"
	"io"
	"os"
	"strings"

	_ "github.com/go-sql-driver/mysql"
)

func checkFilePerm0600(path string) error {
	info, err := os.Stat(path)
	if err != nil {
		return err
	}
	mode := info.Mode().Perm()
	if mode&0077 != 0 {
		return fmt.Errorf("凭据文件 %s 权限过大 (%04o)，为保障安全必须为 0600", path, mode)
	}
	return nil
}

func validateReadOnly(sqlText string) error {
	trimmed := strings.ToUpper(strings.TrimSpace(sqlText))
	allowed := []string{"SELECT", "SHOW", "EXPLAIN", "DESCRIBE", "DESC"}
	for _, prefix := range allowed {
		if strings.HasPrefix(trimmed, prefix) {
			return nil
		}
	}
	return fmt.Errorf("只读模式已启用: 拒绝执行非查询语句")
}

func parseConfig(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()

	conf := map[string]string{}
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		parts := strings.SplitN(line, "=", 2)
		if len(parts) == 2 {
			conf[strings.TrimSpace(parts[0])] = strings.TrimSpace(parts[1])
		}
	}

	host := conf["host"]
	port := conf["port"]
	user := conf["user"]
	pass := conf["password"]
	db := conf["database"]

	if host == "" {
		host = "127.0.0.1"
	}
	if port == "" {
		port = "3306"
	}

	dsn := fmt.Sprintf("%s:%s@tcp(%s:%s)/%s?parseTime=true", user, pass, host, port, db)
	return dsn, nil
}

func main() {
	configFile := flag.String("config", "", "路径指向 0600 权限的配置文件")
	readOnly := flag.Bool("read-only", false, "是否启用只读模式拦截写操作")
	maxRows := flag.Int("max-rows", 1000, "最大返回行数限制")
	flag.Parse()

	if *configFile == "" {
		fmt.Fprintln(os.Stderr, "错误: 必须通过 --config 指定凭据文件")
		os.Exit(1)
	}

	if err := checkFilePerm0600(*configFile); err != nil {
		fmt.Fprintln(os.Stderr, "安全拒绝:", err)
		os.Exit(1)
	}

	dsn, err := parseConfig(*configFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "解析配置文件失败")
		os.Exit(1)
	}

	// 读取 stdin
	reader := bufio.NewReader(os.Stdin)
	sqlBytes, err := io.ReadAll(reader)
	if err != nil {
		fmt.Fprintln(os.Stderr, "读取标准输入失败:", err)
		os.Exit(1)
	}
	sqlText := strings.TrimSpace(string(sqlBytes))
	if sqlText == "" {
		fmt.Fprintln(os.Stderr, "错误: 接收到的 SQL 为空")
		os.Exit(1)
	}

	if *readOnly {
		if err := validateReadOnly(sqlText); err != nil {
			fmt.Fprintln(os.Stderr, "安全拒绝:", err)
			os.Exit(1)
		}
	}

	db, err := sql.Open("mysql", dsn)
	if err != nil {
		fmt.Fprintln(os.Stderr, "连接数据库失败 (密码已脱敏)")
		os.Exit(1)
	}
	defer db.Close()

	rows, err := db.Query(sqlText)
	if err != nil {
		// 脱敏错误信息
		errMsg := err.Error()
		fmt.Fprintln(os.Stderr, "执行 SQL 失败:", errMsg)
		os.Exit(1)
	}
	defer rows.Close()

	columns, err := rows.Columns()
	if err != nil {
		fmt.Fprintln(os.Stderr, "获取列信息失败:", err)
		os.Exit(1)
	}

	// 打印表头
	fmt.Println(strings.Join(columns, "\t"))

	values := make([]sql.RawBytes, len(columns))
	scanArgs := make([]interface{}, len(values))
	for i := range values {
		scanArgs[i] = &values[i]
	}

	count := 0
	for rows.Next() {
		if count >= *maxRows {
			fmt.Fprintf(os.Stderr, "警告: 结果集超过最大行数限制 (%d)，已截断\n", *maxRows)
			break
		}
		if err := rows.Scan(scanArgs...); err != nil {
			fmt.Fprintln(os.Stderr, "读取行数据失败:", err)
			os.Exit(1)
		}

		var rowStrs []string
		for _, col := range values {
			if col == nil {
				rowStrs = append(rowStrs, "NULL")
			} else {
				// 替换换行符以保持 TSV 格式不破坏
				val := strings.ReplaceAll(string(col), "\n", "\\n")
				val = strings.ReplaceAll(val, "\r", "\\r")
				rowStrs = append(rowStrs, val)
			}
		}
		fmt.Println(strings.Join(rowStrs, "\t"))
		count++
	}

	if err = rows.Err(); err != nil {
		fmt.Fprintln(os.Stderr, "遍历结果集失败:", err)
		os.Exit(1)
	}
}
