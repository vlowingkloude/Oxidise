cd frontend

npm install

npm run build

cd ..

cargo run -p oxidize-server -- ~/workspace --host 0.0.0.0
