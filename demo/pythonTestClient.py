from ciaos import Ciaos, Config

def main():
    config = Config(
        api_url="http://localhost:9710",
        user_id="testuser1",
        user_access_key="your_access_key_here"
    )
    client = Ciaos(config)

    # 1. PUT: Upload a file
    print("Uploading file...")
    put_response = client.put("test.txt", key="testkey")
    print("PUT response:", getattr(put_response, "text", put_response))

    # 2. PUT_BINARY: Upload binary data
    print("Uploading binary data...")
    data_list = [b"binary data 1", b"binary data 2"]
    put_bin_response = client.put_binary("binarykey", data_list)
    print("PUT_BINARY response:", getattr(put_bin_response, "text", put_bin_response))

    # 3. GET: Retrieve file(s)
    print("Retrieving file...")
    files = client.get("testkey")
    print("GET response:", files)

    # 4. UPDATE: Update data for a key
    print("Updating file...")
    update_response = client.update("testkey", [b"updated data"])
    print("UPDATE response:", getattr(update_response, "text", update_response))

    # 5. APPEND: Append data to a key
    print("Appending data...")
    append_response = client.append("testkey", [b"appended data"])
    print("APPEND response:", append_response)

    # 6. UPDATE_KEY: Change the key for existing data
    print("Updating key...")
    update_key_response = client.update_key("testkey", "newtestkey")
    print("UPDATE_KEY response:", update_key_response)

    # 7. DELETE: Delete data by key
    print("Deleting file...")
    delete_response = client.delete("newtestkey")
    print("DELETE response:", delete_response)

if __name__ == "__main__":
    main()
