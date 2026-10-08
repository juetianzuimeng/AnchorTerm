package main

import (
	"archive/zip"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"

	"gopkg.in/yaml.v3"
)

// ConfigSource stores the merged configuration
type ConfigStore struct {
	data   map[string]interface{}
	source string
}

func NewConfigStore() *ConfigStore {
	return &ConfigStore{
		data: make(map[string]interface{}),
	}
}

// Flatten a nested map into dot-notation map
func flattenMap(prefix string, v interface{}, flat map[string]string) {
	switch child := v.(type) {
	case map[string]interface{}:
		for k, val := range child {
			newKey := k
			if prefix != "" {
				newKey = prefix + "." + k
			}
			flattenMap(newKey, val, flat)
		}
	case map[interface{}]interface{}:
		for k, val := range child {
			keyStr := fmt.Sprintf("%v", k)
			newKey := keyStr
			if prefix != "" {
				newKey = prefix + "." + keyStr
			}
			flattenMap(newKey, val, flat)
		}
	default:
		flat[prefix] = fmt.Sprintf("%v", v)
	}
}

func parseYaml(content []byte) (map[string]string, error) {
	var parsed interface{}
	err := yaml.Unmarshal(content, &parsed)
	if err != nil {
		return nil, err
	}
	flat := make(map[string]string)
	flattenMap("", parsed, flat)
	return flat, nil
}

func (cs *ConfigStore) loadFromBytes(content []byte, sourceId string) {
	flat, err := parseYaml(content)
	if err != nil {
		return
	}
	if len(flat) > 0 {
		cs.source = sourceId
		for k, v := range flat {
			cs.data[k] = v
		}
	}
}

func (cs *ConfigStore) loadFromFile(path string) {
	content, err := os.ReadFile(path)
	if err == nil {
		cs.loadFromBytes(content, "file:"+path)
	}
}

func (cs *ConfigStore) loadFromJar(jarPath string, entryName string) {
	r, err := zip.OpenReader(jarPath)
	if err != nil {
		return
	}
	defer r.Close()

	for _, f := range r.File {
		if f.Name == entryName {
			rc, err := f.Open()
			if err != nil {
				return
			}
			content, err := io.ReadAll(rc)
			rc.Close()
			if err == nil {
				cs.loadFromBytes(content, "jar:"+entryName)
			}
			return
		}
	}
}

func extractDbFromUrl(url string) (host string, port int, db string) {
	host = "127.0.0.1"
	port = 3306
	if url == "" {
		return
	}
	re := regexp.MustCompile(`jdbc:mysql://([^:/]+)(?::(\d+))?/([^?]+)`)
	matches := re.FindStringSubmatch(url)
	if len(matches) > 3 {
		host = matches[1]
		if matches[2] != "" {
			port, _ = strconv.Atoi(matches[2])
		}
		db = matches[3]
	}
	return
}

func getActiveProfiles(cs *ConfigStore) []string {
	var profilesStr string
	if val, ok := cs.data["spring.profiles.active"]; ok {
		profilesStr = val.(string)
	}
	if envVal, exists := os.LookupEnv("SPRING_PROFILES_ACTIVE"); exists {
		profilesStr = envVal
	}
	var profiles []string
	for _, p := range strings.Split(profilesStr, ",") {
		p = strings.TrimSpace(p)
		if p != "" {
			profiles = append(profiles, p)
		}
	}
	return profiles
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "Usage: spring-connection <work_dir>")
		os.Exit(1)
	}
	workDir := os.Args[1]
	os.Chdir(workDir)

	var fatJar string
	files, _ := os.ReadDir(".")
	for _, f := range files {
		if !f.IsDir() && strings.HasSuffix(f.Name(), ".jar") {
			fatJar = f.Name()
			break
		}
	}

	cs := NewConfigStore()

	loadAndMerge := func(target string, profile string) {
		name := target
		if profile != "" {
			name = strings.TrimSuffix(target, ".yml") + "-" + profile + ".yml"
		}
		if fatJar != "" {
			cs.loadFromJar(fatJar, "BOOT-INF/classes/"+name)
			cs.loadFromJar(fatJar, "BOOT-INF/classes/config/"+name)
		}
		cs.loadFromFile(name)
		cs.loadFromFile("config/" + name)
	}

	// 1. Base configs
	loadAndMerge("application.yml", "")

	// 2. Profile configs
	for _, p := range getActiveProfiles(cs) {
		loadAndMerge("application.yml", p)
	}

	// 3. Env overrides
	envMap := map[string]string{
		"SPRING_DATASOURCE_URL":      "spring.datasource.url",
		"SPRING_DATASOURCE_USERNAME": "spring.datasource.username",
		"SPRING_DATASOURCE_PASSWORD": "spring.datasource.password",
		"SPRING_DATA_REDIS_HOST":     "spring.redis.host",
		"SPRING_REDIS_HOST":          "spring.redis.host",
		"SPRING_DATA_REDIS_PORT":     "spring.redis.port",
		"SPRING_REDIS_PORT":          "spring.redis.port",
		"SPRING_DATA_REDIS_PASSWORD": "spring.redis.password",
		"SPRING_REDIS_PASSWORD":      "spring.redis.password",
		"SPRING_DATA_REDIS_DATABASE": "spring.redis.database",
		"SPRING_REDIS_DATABASE":      "spring.redis.database",
	}

	envUsed := false
	for envKey, confKey := range envMap {
		if val, exists := os.LookupEnv(envKey); exists {
			cs.data[confKey] = val
			envUsed = true
		}
	}
	if envUsed {
		cs.source = "env:environment_variables"
	}

	// Generate credentials
	pid := os.Getpid()
	mysqlCred := fmt.Sprintf("/tmp/.at_cred_mysql_%d.conf", pid)
	redisCred := fmt.Sprintf("/tmp/.at_cred_redis_%d.conf", pid)

	// Fallback to workDir if /tmp is not writable
	if err := os.WriteFile(mysqlCred+".test", []byte(""), 0600); err != nil {
		mysqlCred = filepath.Join(workDir, fmt.Sprintf(".at_cred_mysql_%d.conf", pid))
		redisCred = filepath.Join(workDir, fmt.Sprintf(".at_cred_redis_%d.conf", pid))
	} else {
		os.Remove(mysqlCred + ".test")
	}

	writeCred := func(path string, content string) {
		os.WriteFile(path, []byte(content), 0600)
	}

	// MySQL Output
	mysqlUrl, _ := cs.data["spring.datasource.url"].(string)
	mysqlUser, _ := cs.data["spring.datasource.username"].(string)
	mysqlPass, _ := cs.data["spring.datasource.password"].(string)
	host, port, db := extractDbFromUrl(mysqlUrl)

	mysqlCredOut := ""
	if mysqlUrl != "" {
		conf := fmt.Sprintf("host=%s\nport=%d\ndatabase=%s\nuser=%s\npassword=%s\n", host, port, db, mysqlUser, mysqlPass)
		writeCred(mysqlCred, conf)
		mysqlCredOut = mysqlCred
	}

	// Redis Output
	getString := func(keys ...string) string {
		for _, k := range keys {
			if val, ok := cs.data[k].(string); ok && val != "" {
				return val
			}
		}
		return ""
	}

	redisHostStr := getString("spring.data.redis.host", "spring.redis.host")
	redisPortStr := getString("spring.data.redis.port", "spring.redis.port")
	redisDbStr := getString("spring.data.redis.database", "spring.redis.database")
	redisPass := getString("spring.data.redis.password", "spring.redis.password")

	redisHost := "127.0.0.1"
	if redisHostStr != "" {
		redisHost = redisHostStr
	}
	redisPort := 6379
	if p, err := strconv.Atoi(redisPortStr); err == nil && p != 0 {
		redisPort = p
	}
	redisDb := "0"
	if redisDbStr != "" {
		redisDb = redisDbStr
	}

	redisCredOut := ""
	if redisHostStr != "" {
		conf := fmt.Sprintf("host=%s\nport=%d\ndatabase=%s\npassword=%s\n", redisHost, redisPort, redisDb, redisPass)
		writeCred(redisCred, conf)
		redisCredOut = redisCred
	}

	output := map[string]interface{}{
		"mysql": map[string]interface{}{
			"config_source":   cs.source,
			"host":            host,
			"port":            port,
			"database":        db,
			"credential_file": nil,
		},
		"redis": map[string]interface{}{
			"config_source":   cs.source,
			"host":            redisHost,
			"port":            redisPort,
			"database":        redisDb,
			"credential_file": nil,
		},
	}

	if mysqlCredOut != "" {
		output["mysql"].(map[string]interface{})["credential_file"] = mysqlCredOut
	}
	if redisCredOut != "" {
		output["redis"].(map[string]interface{})["credential_file"] = redisCredOut
	}

	jsonBytes, _ := json.MarshalIndent(output, "", "  ")
	fmt.Println(string(jsonBytes))
}
