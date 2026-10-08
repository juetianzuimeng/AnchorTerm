package main

import (
	"bufio"
	"context"
	"encoding/csv"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/redis/go-redis/v9"
)

func checkFilePerm0600(path string) error {
	info, err := os.Stat(path)
	if err != nil {
		return err
	}
	if info.Mode().Perm()&0077 != 0 {
		return fmt.Errorf("credential file permissions are not 0600: %v", info.Mode().Perm())
	}
	return nil
}

func parseConfig(path string) (map[string]string, error) {
	if err := checkFilePerm0600(path); err != nil {
		return nil, err
	}
	content, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	conf := make(map[string]string)
	lines := strings.Split(string(content), "\n")
	for _, line := range lines {
		line = strings.TrimSpace(line)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		parts := strings.SplitN(line, "=", 2)
		if len(parts) == 2 {
			conf[parts[0]] = parts[1]
		}
	}
	return conf, nil
}

func parseCommand(cmdStr string) ([]interface{}, error) {
	r := csv.NewReader(strings.NewReader(cmdStr))
	r.Comma = ' '
	r.LazyQuotes = true
	record, err := r.Read()
	if err != nil {
		parts := strings.Fields(cmdStr)
		var args []interface{}
		for _, p := range parts {
			args = append(args, p)
		}
		return args, nil
	}
	var args []interface{}
	for _, p := range record {
		if p != "" {
			args = append(args, p)
		}
	}
	return args, nil
}

func isReadOnly(cmd string) bool {
	cmd = strings.ToUpper(cmd)
	allowed := map[string]bool{
		"GET": true, "MGET": true, "HGET": true, "HMGET": true, "HGETALL": true, "HKEYS": true, "HVALS": true,
		"SMEMBERS": true, "SISMEMBER": true, "SCARD": true, "SRANDMEMBER": true,
		"LRANGE": true, "LINDEX": true, "LLEN": true,
		"ZRANGE": true, "ZREVRANGE": true, "ZRANGEBYSCORE": true, "ZREVRANGEBYSCORE": true, "ZCARD": true, "ZSCORE": true, "ZCOUNT": true,
		"TYPE": true, "TTL": true, "PTTL": true, "EXISTS": true, "STRLEN": true,
		"KEYS": true, "SCAN": true, "HSCAN": true, "SSCAN": true, "ZSCAN": true,
		"INFO": true, "PING": true, "DBSIZE": true,
	}
	return allowed[cmd]
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "Usage: at-redis <credential_file> [--read-only]")
		os.Exit(1)
	}
	credFile := os.Args[1]
	readOnly := false
	for _, arg := range os.Args {
		if arg == "--read-only" {
			readOnly = true
		}
	}

	conf, err := parseConfig(credFile)
	if err != nil {
		fmt.Fprintln(os.Stderr, "Error:", err)
		os.Exit(1)
	}

	dbInt := 0
	if conf["database"] != "" {
		dbInt, _ = strconv.Atoi(conf["database"])
	}
	addr := conf["host"] + ":" + conf["port"]

	opts := &redis.Options{
		Addr:     addr,
		Password: conf["password"],
		DB:       dbInt,
	}
	client := redis.NewClient(opts)
	defer client.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	if err := client.Ping(ctx).Err(); err != nil {
		fmt.Fprintln(os.Stderr, "Error connecting to Redis (password masked):", err)
		os.Exit(1)
	}

	scanner := bufio.NewScanner(os.Stdin)
	if scanner.Scan() {
		cmdStr := scanner.Text()
		args, _ := parseCommand(cmdStr)
		if len(args) == 0 {
			os.Exit(0)
		}
		
		cmdName := strings.ToUpper(fmt.Sprintf("%v", args[0]))
		if readOnly && !isReadOnly(cmdName) {
			fmt.Fprintf(os.Stderr, "Error: read-only mode is enabled, command '%s' is not allowed.\n", cmdName)
			os.Exit(1)
		}

		res, err := client.Do(ctx, args...).Result()
		if err == redis.Nil {
			fmt.Println("null")
			os.Exit(0)
		} else if err != nil {
			fmt.Fprintln(os.Stderr, "Error executing command:", err)
			os.Exit(1)
		}
		
		switch v := res.(type) {
		case []byte:
			fmt.Println(string(v))
		case string:
			fmt.Println(v)
		case int64:
			fmt.Println(v)
		default:
			j, err := json.MarshalIndent(res, "", "  ")
			if err != nil {
				fmt.Printf("%v\n", res)
			} else {
				fmt.Println(string(j))
			}
		}
	}
}
